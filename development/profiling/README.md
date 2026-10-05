# alp / slp によるローカル計測

Rust のローカル構成は `localhost:8080 → nginx → webapp:8080 → MySQL` です。内部 matcher も nginx を経由します。ベンチマーカーのソースを変更せずに計測します。

## 実行

Docker / Colima、Go、Task、Python 3.9 以上、curl を使用します。リポジトリのルートで実行します。

```sh
task profile:bench
```

このコマンドは **DB を初期化** し、Rust を release ビルド・起動して、通常の 60 秒ベンチマークを 1 回実行します。計測中はブラウザ操作や他のベンチマークを控えてください。

- alp v1.0.22 / slp v0.2.3 の公式配布バイナリを SHA-256 検証後、`_tmp/profile-tools/bin/` に配置します。macOS / Linux の arm64 / amd64 に対応します。
- nginx は JSON アクセスログを標準出力へ記録します。alp はリクエスト全体の `request_time` を秒単位で集計します（元ログの分解能はミリ秒）。URL のクエリ文字列は除き、ride ID を含む 2 ルートをまとめます。
- MySQL は一時的に `long_query_time=0`、`min_examined_row_limit=0`、`log_slow_extra=ON` として SQL を記録します。SQLx の既存接続にも設定を反映するため、計測前に Rust コンテナを再起動します。
- nginx 再読み込み・アプリの疎通確認後、matcher も再起動して古い接続先への待機を解消します。配車呼び出しの 0.5 秒間隔は変更しません。
- ベンチマークの成否にかかわらず、終了時に MySQL の元の設定を復元し、Rust を再起動して接続の設定も戻します。DB コンテナは再起動しません。

通常の起動はこれまでどおり `task rust:run` です。全クエリ記録は `profile:bench` の実行中だけ有効です。

## 集計区間と出力

結果は `_tmp/profiles/YYYYMMDD-HHMMSS/` に保存します。Git 管理対象外です。

| ファイル | 内容 |
| --- | --- |
| `bench.log` | 変更していないベンチマーカーの出力、スコア、エラー |
| `window.json` | 実際に集計した開始・終了時刻、秒数、HTTP / SQL 件数、slow log レコード数 |
| `alp.md` / `alp.csv` | HTTP の累積時間降順。回数・平均・p95・p99・最大・ステータス別件数 |
| `slp.md` / `slp.csv` | SQL の累積時間降順。回数・平均・p95・最大・ロック時間・走査行数 |
| `access.jsonl` / `slow.log` | 集計区間だけを抜き出した入力ログ |
| `nginx.log` / `mysql-slow.log` | 初期化・検証も含む元ログ |
| `metadata.json` | コマンド、コミット、ツール版、MySQL 設定の復元結果、時計差の概算 |
| `file-io-before.tsv` / `file-io-after.tsv` | ベンチマーク起動前・終了後の Performance Schema ファイル I/O 累積値 |
| `restore-mysql.sql` | 強制終了などで自動復元できなかった場合の復元用 SQL |

ベンチマーカーを変更せず、既存の `時間経過 tick=...` の **最初と最後のログの間** を抽出します。60 秒走行のうち通常は約 57.6 秒です。初期化・事前検証・事後検証と、負荷走行の両端の少しの時間を除きます。正確な区間は `window.json` を確認してください。

HTTP は nginx の `$msec`、SQL は MySQL の `End` を使い、同じ `[開始, 終了)` の **完了時刻** で抽出します。境界をまたいだリクエストとその SQL は異なる側に入ることがあります。内部 matcher のリクエストも含みます。

**SSE の `request_time` は接続の継続時間で、通知の遅延ではありません。** 通知 API の alp 平均・p95 をポーリング時の応答時間と比較しないでください。集計終了時も接続中の SSE は alp に入りません。元の `nginx.log` にある `content_type=text/event-stream` を使い、`完了時刻 >= 集計開始` かつ `完了時刻 - response_time < 集計終了` の接続を数えると、区間に重なる SSE 接続数を別途確認できます。メッセージ数や配信遅延の計測には別の指標が必要です。

ファイル I/O は `performance_schema.file_summary_by_event_name` を読み、カウンターのリセットや計測器の有効化は行いません。前後の差分が **初期化・事前検証・事後検証を含むベンチマーク全体** の値で、alp / slp の区間とは異なります。`SUM_TIMER_*` はピコ秒（秒への変換は 10^12 で除算）。`MISC` は同期など複数種類の操作を含み、すべてを fsync と見なしたり、特定 API の COMMIT 待ちと断定したりできません。[MySQL 公式資料](https://dev.mysql.com/doc/refman/8.4/en/performance-schema-file-summary-tables.html)

slow log には SQL 以外に Ping / Prepare / Quit などのプロトコル操作も記録されます。slp はそれらを除外するため、元レコード数と SQL 集計件数を別々に記録します。CSV は slp の TSV 出力から変換し、SQL 中のカンマも正しく引用します。

累積時間は並行処理した各リクエスト・SQL の合計なので、実際の経過時間を超えます。alp と slp は個々の HTTP と SQL を紐付けません。HTTP にはアプリ側の処理や DB 接続待ちなども含まれ、SQL 時間との差を特定の待ち時間と断定できません。

nginx と全 SQL のログ出力により計測負荷が加わるため、ログを無効にしたスコアと直接比較しません。

## 再集計とツールの直接利用

```sh
task profile:install
task profile:analyze -- _tmp/profiles/YYYYMMDD-HHMMSS
task alp -- json --file _tmp/profiles/YYYYMMDD-HHMMSS/access.jsonl --sort sum -r
task slp -- my --file _tmp/profiles/YYYYMMDD-HHMMSS/slow.log --sort sum-query-time -r
```

`task alp` / `task slp` の代わりに `_tmp/profile-tools/bin/alp` / `_tmp/profile-tools/bin/slp` を直接実行することもできます。

強制終了で設定が残った場合は、その実行の復元用 SQL を使います。

```sh
docker compose -f development/compose-local.yml exec -T -e MYSQL_PWD=isucon db mysql -uroot < _tmp/profiles/YYYYMMDD-HHMMSS/restore-mysql.sql
docker compose -f development/compose-local.yml -f development/compose-rust-local.yml restart webapp
```

ログ抽出の検証:

```sh
python3 -m unittest discover -s development/profiling -p 'test_*.py'
```

## 公式資料

- [alp](https://github.com/tkuchiki/alp)
- [slp](https://github.com/tkuchiki/slp)
- [nginx のログ形式と request_time](https://nginx.org/en/docs/http/ngx_http_log_module.html)
- [MySQL 8.4 の slow query log](https://dev.mysql.com/doc/refman/8.4/en/slow-query-log.html)
