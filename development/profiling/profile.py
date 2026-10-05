#!/usr/bin/env python3
"""Local Rust benchmark profiling. Raw logs and downloaded tools stay in _tmp."""

import argparse
import csv
from datetime import datetime, timedelta, timezone
import fcntl
import hashlib
import json
from pathlib import Path
import platform
import re
import signal
import subprocess
import tarfile
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
BIN = ROOT / "_tmp/profile-tools/bin"
COMPOSE = ["docker", "compose", "-f", str(ROOT / "development/compose-local.yml"),
           "-f", str(ROOT / "development/compose-rust-local.yml")]
VARIABLES = ["slow_query_log", "slow_query_log_file", "long_query_time",
             "min_examined_row_limit", "log_output", "log_slow_extra", "log_timestamps"]


def command(args, **kwargs):
    return subprocess.run([str(arg) for arg in args], check=True, **kwargs)


def output(args):
    return command(args, capture_output=True, text=True).stdout.strip()


def mysql(sql):
    return output(COMPOSE + ["exec", "-T", "-e", "MYSQL_PWD=isucon", "db", "mysql",
                             "-uroot", "-N", "-B", "-e", sql])


def install():
    os_name = platform.system().lower()
    arch = {"arm64": "arm64", "aarch64": "arm64", "x86_64": "amd64"}.get(platform.machine())
    if os_name not in ("darwin", "linux") or arch is None:
        raise RuntimeError("Supported platforms: macOS/Linux arm64/amd64")
    BIN.mkdir(parents=True, exist_ok=True)
    for name, version, checksum_file in [
        ("alp", "1.0.22", "alp_1.0.22_checksums.txt"),
        ("slp", "0.2.3", "slp_v0.2.3_checksums.txt"),
    ]:
        dest = BIN / name
        if dest.exists() and version in output([dest, "--version"]):
            continue
        archive_name = f"{name}_{os_name}_{arch}.tar.gz"
        base = f"https://github.com/tkuchiki/{name}/releases/download/v{version}"
        with tempfile.TemporaryDirectory() as tmp:
            archive = Path(tmp) / archive_name
            checksums = output(["curl", "-fsSL", "--retry", "3", f"{base}/{checksum_file}"])
            expected = next(line.split()[0] for line in checksums.splitlines()
                            if line.split()[-1] == archive_name)
            command(["curl", "-fsSL", "--retry", "3", f"{base}/{archive_name}", "-o", archive])
            if hashlib.sha256(archive.read_bytes()).hexdigest() != expected:
                raise RuntimeError(f"Checksum mismatch: {archive_name}")
            with tarfile.open(archive) as bundle:
                member = next(m for m in bundle.getmembers() if Path(m.name).name == name and m.isfile())
                dest.write_bytes(bundle.extractfile(member).read())
            dest.chmod(0o755)
        print(output([dest, "--version"]), flush=True)


def load_window(log, started_at):
    # Use existing progress messages only: exclude preparation and validation,
    # and explicitly omit the unsampled edges of the 60-second load phase.
    jst = timezone(timedelta(hours=9))
    started = datetime.fromisoformat(started_at).astimezone(jst)
    progress = []
    for line in log.splitlines():
        if "msg=時間経過 tick=" not in line:
            continue
        match = re.search(r'time=(\d{2}:\d{2}:\d{2}\.\d{3})', line)
        if not match:
            raise RuntimeError("Unexpected benchmark progress timestamp")
        stamp = datetime.combine(started.date(), datetime.strptime(match[1], "%H:%M:%S.%f").time(), jst)
        if stamp < started:
            stamp += timedelta(days=1)
        progress.append(stamp.timestamp())
    if len(progress) < 2 or progress[-1] <= progress[0]:
        raise RuntimeError("Benchmark did not record enough load progress messages")
    return progress[0], progress[-1]


def filter_access(source, dest, start, end):
    count = 0
    with source.open() as src, dest.open("w") as dst:
        for line in src:
            # Docker startup messages are not access records.
            if not line.startswith("{"):
                continue
            record = json.loads(line)
            if start <= record["msec"] < end:
                dst.write(line)
                count += 1
    return count


def slow_records(source):
    record = []
    for line in source:
        if line.startswith("# Time:"):
            if record:
                yield "".join(record)
            record = [line]
        elif record:
            record.append(line)
    if record:
        yield "".join(record)


def filter_slow(source, dest, start, end):
    count = 0
    with source.open() as src, dest.open("w") as dst:
        for record in slow_records(src):
            # End is microsecond precision and is emitted by log_slow_extra.
            match = re.search(r'\bEnd: (\S+)', record)
            if not match:
                raise RuntimeError("Slow log is missing End timestamps; enable log_slow_extra")
            completed = datetime.fromisoformat(match[1].replace("Z", "+00:00")).timestamp()
            if start <= completed < end:
                dst.write(record)
                count += 1
    return count


def analyze(directory):
    directory = directory.resolve()
    metadata = json.loads((directory / "metadata.json").read_text())
    start, end = load_window((directory / "bench.log").read_text(), metadata["started_at"])
    http_count = filter_access(directory / "nginx.log", directory / "access.jsonl", start, end)
    sql_count = filter_slow(directory / "mysql-slow.log", directory / "slow.log", start, end)
    if not http_count or not sql_count:
        raise RuntimeError(f"Empty profile: HTTP={http_count}, SQL={sql_count}")
    summary = {"load_start": datetime.fromtimestamp(start, timezone.utc).isoformat(),
               "load_end": datetime.fromtimestamp(end, timezone.utc).isoformat(),
               "duration_seconds": end - start, "http_records": http_count, "slow_log_records": sql_count,
               "window": "first to last existing tick progress log; completion time >= start and < end; includes internal matcher; excludes load edges"}
    alp = [BIN / "alp", "json", "--file", directory / "access.jsonl", "--nosave-pos",
           "--sort", "sum", "-r", "--percentiles", "95,99",
           "-m", r"^/api/app/rides/[^/]+/evaluation$,^/api/chair/rides/[^/]+/status$",
           "-o", "count,2xx,4xx,5xx,method,uri,sum,avg,p95,p99,max"]
    slp = [BIN / "slp", "my", "--file", directory / "slow.log", "--nosave-pos",
           "--sort", "sum-query-time", "-r", "--percentiles", "95,99",
           "-o", "count,query,sum-query-time,avg-query-time,p95-query-time,max-query-time,"
                 "sum-lock-time,sum-rows-examined,avg-rows-examined,sum-rows-sent"]
    for name, args in [("alp", alp), ("slp", slp)]:
        # slp's CSV output does not quote commas inside SQL. Convert its TSV
        # with the standard CSV writer so queries remain one column.
        for fmt, suffix in [("markdown", "md"), ("tsv", "tsv")]:
            with (directory / f"{name}.{suffix}").open("w") as dest:
                command(args + ["--format", fmt], stdout=dest)
        with (directory / f"{name}.tsv").open() as src, (directory / f"{name}.csv").open("w", newline="") as dst:
            csv.writer(dst, lineterminator="\n").writerows(csv.reader(src, delimiter="\t"))
    with (directory / "slp.csv").open() as src:
        sql_rows = list(csv.DictReader(src))
    summary["sql_statements"] = sum(int(row["Count"]) for row in sql_rows)
    summary["sql_groups"] = len(sql_rows)
    summary["slow_log_records_not_aggregated_by_slp"] = sql_count - summary["sql_statements"]
    (directory / "window.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, ensure_ascii=False, indent=2))
    print(f"Results: {directory}")


def restore_sql(values):
    # Values originate from MySQL globals, not shell interpolation.
    def quoted(value):
        return "'" + value.replace("\\", "\\\\").replace("'", "''") + "'"
    statements = ["SET GLOBAL slow_query_log=OFF"]
    numeric = {"long_query_time", "min_examined_row_limit", "log_slow_extra"}
    statements += [f"SET GLOBAL {key}={values[key] if key in numeric else quoted(values[key])}"
                   for key in VARIABLES[1:]]
    statements.append(f"SET GLOBAL slow_query_log={values['slow_query_log']}")
    return ";\n".join(statements) + ";\n"


def bench():
    runs = ROOT / "_tmp/profiles"
    runs.mkdir(parents=True, exist_ok=True)
    # Prevent two profiling runs from changing global MySQL settings concurrently.
    with (runs / ".lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        run_benchmark(runs)


def run_benchmark(runs):
    directory = runs / datetime.now().strftime("%Y%m%d-%H%M%S")
    directory.mkdir()
    print(f"Results: {directory}", flush=True)
    executable = directory / "bench"
    command(["go", "build", "-o", executable, "."], cwd=ROOT / "bench")
    saved = dict(zip(VARIABLES, mysql("SELECT " + ",".join("@@GLOBAL." + v for v in VARIABLES)).split("\t")))
    recovery = restore_sql(saved)
    (directory / "restore-mysql.sql").write_text(recovery)
    slow_path = f"/var/lib/mysql/profile-{directory.name}.slow.log"
    since = datetime.now(timezone.utc).isoformat()
    args = [str(executable), "run", "--target", "http://localhost:8080", "-t", "60",
            "--payment-bind-port", "12346", "--payment-url", "http://host.docker.internal:12346",
            "--skip-static-sanity-check", "--fail-on-error"]
    before = time.time()
    db_clock = float(mysql("SELECT UNIX_TIMESTAMP(NOW(6))"))
    after = time.time()
    metadata = {"started_at": since, "git_commit": output(["git", "rev-parse", "HEAD"]), "command": args,
                "mysql_before": saved, "mysql_version": mysql("SELECT @@version"),
                "alp": output([BIN / "alp", "--version"]), "slp": output([BIN / "slp", "--version"]),
                "clock_offset_estimate_seconds": db_clock - (before + after) / 2,
                "clock_measurement_roundtrip_seconds": after - before}
    (directory / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    process = None
    try:
        mysql(f"SET GLOBAL slow_query_log=OFF; SET GLOBAL log_output='FILE'; "
              f"SET GLOBAL slow_query_log_file='{slow_path}'; SET GLOBAL long_query_time=0; "
              "SET GLOBAL min_examined_row_limit=0; SET GLOBAL log_slow_extra=ON; "
              "SET GLOBAL log_timestamps='UTC'; SET GLOBAL slow_query_log=ON;")
        # Session variables are copied at connect time. Refresh all SQLx connections.
        command(COMPOSE + ["restart", "webapp"])
        command(COMPOSE + ["exec", "-T", "nginx", "nginx", "-t"])
        command(COMPOSE + ["exec", "-T", "nginx", "nginx", "-s", "reload"])
        # An unauthenticated endpoint returns 401 once the app is ready.
        for _ in range(30):
            probe = subprocess.run(["curl", "-s", "--max-time", "2", "-o", "/dev/null", "-w", "%{http_code}",
                                    "http://localhost:8080/api/app/notification"], capture_output=True, text=True)
            if probe.stdout == "401":
                break
            time.sleep(1)
        else:
            raise RuntimeError("Rust app did not become ready behind nginx")
        print("Running 60-second benchmark (database will be initialized)...", flush=True)
        with (directory / "bench.log").open("w") as log:
            process = subprocess.Popen(args, cwd=ROOT / "bench", stdout=subprocess.PIPE,
                                       stderr=subprocess.STDOUT, text=True, bufsize=1)
            for line in process.stdout:
                log.write(line)
                log.flush()
                print(line, end="", flush=True)
            returncode = process.wait()
        metadata["benchmark_exit_code"] = returncode
        (directory / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    finally:
        if process is not None and process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        # Restore even on benchmark failure or Ctrl-C. Never restart/delete the DB.
        mysql(recovery)
        command(COMPOSE + ["restart", "webapp"])
        command(COMPOSE + ["exec", "-T", "nginx", "nginx", "-s", "reload"])
        metadata["mysql_after"] = dict(zip(VARIABLES, mysql("SELECT " + ",".join("@@GLOBAL." + v for v in VARIABLES)).split("\t")))
        (directory / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
        with (directory / "nginx.log").open("w") as dest:
            command(COMPOSE + ["logs", "--no-log-prefix", "--no-color", "--since", since, "nginx"], stdout=dest, stderr=subprocess.STDOUT)
        db_id = output(COMPOSE + ["ps", "-q", "db"])
        command(["docker", "cp", f"{db_id}:{slow_path}", directory / "mysql-slow.log"])
        command(COMPOSE + ["exec", "-T", "db", "rm", "--", slow_path])
    analyze(directory)
    if returncode:
        raise SystemExit(returncode)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="action", required=True)
    sub.add_parser("install")
    sub.add_parser("bench")
    sub.add_parser("analyze").add_argument("directory", type=Path)
    args = parser.parse_args()
    if args.action == "install":
        install()
    elif args.action == "analyze":
        analyze(args.directory)
    else:
        def interrupted(signum, frame):
            raise KeyboardInterrupt
        signal.signal(signal.SIGTERM, interrupted)
        bench()


if __name__ == "__main__":
    main()
