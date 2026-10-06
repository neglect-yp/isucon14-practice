import io
from pathlib import Path
import tempfile
import unittest

from profile import filter_access, filter_slow, load_window, slow_records


class LogWindowTest(unittest.TestCase):
    def test_progress_window_across_midnight_excludes_prepare_and_validation(self):
        log = "\n".join([
            'time=23:59:55.000 level=INFO msg=負荷走行を開始します',
            'time=23:59:59.000 level=DEBUG source=scenario.go msg=時間経過 tick=60',
            'time=00:00:00.800 level=DEBUG source=scenario.go msg=時間経過 tick=120',
            'time=00:00:07.000 level=INFO msg=結果 pass=true',
        ])
        start, end = load_window(log, "2026-10-05T14:59:54+00:00")
        self.assertAlmostEqual(end - start, 1.8, places=5)
        self.assertEqual(start, 1791212399)

    def test_incomplete_run_is_rejected(self):
        with self.assertRaises(RuntimeError):
            load_window("time=12:00:00.000 msg=負荷走行を開始します", "2026-10-05T03:00:00+00:00")

    def test_completion_filter_is_half_open_and_preserves_multiline_sql(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source, dest = root / "raw", root / "filtered"
            source.write_text('nginx startup\n{"msec":1}\n{"msec":2}\n{"msec":3}\n')
            self.assertEqual(filter_access(source, dest, 2, 3), 1)
            self.assertEqual(dest.read_text(), '{"msec":2}\n')
            records = [f"# Time: 1970-01-01T00:00:0{n}.000000Z\n"
                       f"# Query_time: 0.1 End: 1970-01-01T00:00:0{n}.000000Z\n"
                       f"SET timestamp={n};\nSELECT\n {n};\n" for n in (3, 2, 1)]
            source.write_text("server header\n" + "".join(records))
            self.assertEqual(filter_slow(source, dest, 2, 3), 1)
            self.assertEqual(dest.read_text(), records[1])
            self.assertEqual(len(list(slow_records(io.StringIO(source.read_text())))), 3)


class SlowLogWindowTest(unittest.TestCase):
    def filtered(self, records):
        with tempfile.TemporaryDirectory() as directory:
            source, dest = Path(directory) / "source", Path(directory) / "dest"
            source.write_text(records)
            count = filter_slow(source, dest, 10.0, 20.0)
            return count, dest.read_text()

    def test_excludes_standard_records_provably_outside_window(self):
        count, text = self.filtered(
            "# Time: 1970-01-01T00:00:01Z\n# Query_time: 1.0\nSELECT 1;\n"
            "# Time: 1970-01-01T00:00:12Z\n# Query_time: 1.0 End: 1970-01-01T00:00:13Z\nSELECT 2;\n"
            "# Time: 1970-01-01T00:00:21Z\n# Query_time: 1.0\nSELECT 3;\n"
        )
        self.assertEqual(count, 1)
        self.assertIn("SELECT 2", text)
        self.assertNotIn("SELECT 1", text)
        self.assertNotIn("SELECT 3", text)

    def test_rejects_missing_end_inside_or_overlapping_window(self):
        for start, duration in [(12, 1), (9, 2), (9, 1)]:
            with self.subTest(start=start, duration=duration), self.assertRaises(RuntimeError):
                self.filtered(f"# Time: 1970-01-01T00:00:{start:02}Z\n# Query_time: {duration}.0\nSELECT 1;\n")

    def test_uses_end_timestamp_and_half_open_boundaries(self):
        count, text = self.filtered(
            "# Time: 1970-01-01T00:00:01Z\n# Query_time: 9.0 End: 1970-01-01T00:00:10Z\nSELECT 1;\n"
            "# Time: 1970-01-01T00:00:11Z\n# Query_time: 9.0 End: 1970-01-01T00:00:20Z\nSELECT 2;\n"
        )
        self.assertEqual(count, 1)
        self.assertIn("SELECT 1", text)


if __name__ == "__main__":
    unittest.main()
