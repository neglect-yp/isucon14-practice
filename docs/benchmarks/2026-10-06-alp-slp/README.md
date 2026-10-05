# 2026-10-06 alp / slp 計測

60 秒ベンチマークは **PASS / スコア 8,524**。今回の計測では、通知 API の大量リクエストと `ride_statuses` の繰り返し全件走査が優先調査候補です。アプリケーションや SQL の性能改善は行っていません。

## 条件と集計範囲

- 実行: 2026-10-06 00:14 JST。対象コミット `92eaa9f1` に nginx と計測スクリプトを追加した作業ツリー。
- ベンチマーカーと Rust アプリのソースは変更なし。`git diff --exit-code -- bench webapp/rust webapp/sql` で確認。
- 共通環境は [ベンチマーク記録](../README.md)。nginx 1.28 系を `localhost:8080` の手前に追加。内部 matcher も nginx 経由。
- alp 1.0.22 / slp 0.2.3、MySQL 8.4.11。全クエリを slow log に記録（`long_query_time=0`、`log_slow_extra=ON`）。
- ベンチ設定: 60 秒、静的ファイル検証省略、決済ポート 12346、`--fail-on-error`。
- 集計: **00:14:17.255 以上、00:15:14.855 未満の 57.600 秒**。既存の `時間経過 tick=60` から `tick=1980` まで。準備・初期化・事後検証と負荷走行の両端を除外。
- HTTP は応答完了、SQL は MySQL の `End` 時刻で抽出。ホストと VM の時計差の概算は +20 ms、測定往復 116 ms であり、境界にこの程度の不確実性がある。
- HTTP **77,321 件**（ベンチマーカー 77,226、内部 matcher 95）。全件 2xx。ブラウザ由来の User-Agent はなし。
- SQL **564,654 回 / 75 種類**。元の slow log は 654,366 レコードで、差の 89,712 は Ping 88,282、Prepare 1,423、Quit 7。slp の集計から除外される。
- 警告: `CODE=26`、`total_distance` の反映遅延が 1 件。ベンチマーク終了コード 0、`pass=true`。

ログ出力負荷と nginx の追加があるため、以前の 8,715 との差から性能の改善・悪化を判断しません。以下の累積秒数は並行処理した時間の合計であり、計測の経過秒数ではありません。

## alp: HTTP

全体の累積時間は **2,455.169 秒**。上位 3 API で **92.6%**、通知 2 API だけで **73.8%** を占めます。

| メソッド / URL | 回数 | 累積秒 | 平均 ms | p95 ms | p99 ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| GET `/api/app/notification` | 27,816 | 1,017.889 | 37 | 91 | 140 |
| GET `/api/chair/notification` | 32,451 | 794.808 | 24 | 65 | 102 |
| POST `/api/chair/coordinate` | 15,718 | 461.372 | 29 | 67 | 123 |
| GET `/api/app/nearby-chairs` | 133 | 69.037 | 519 | 1448 | 1704 |
| GET `/api/owner/chairs` | 160 | 59.257 | 370 | 708 | 907 |
| GET `/api/owner/sales` | 131 | 11.882 | 91 | 218 | 279 |
| POST `^/api/app/rides/[^/]+/evaluation$` | 80 | 11.164 | 140 | 510 | 1016 |
| GET `/api/internal/matching` | 95 | 8.979 | 95 | 286 | 717 |

通知は 1 回あたりの p95 が 65–91 ms でも、合計 60,267 回の呼び出しによって累積負荷が大きくなっています。一方、`nearby-chairs` と `owner/chairs` は回数が少なく、1 回あたりが遅い API です。

## slp: SQL

全 SQL の累積時間は **694.544 秒**、走査行数の合計は **861,187,084 行**。主要な `ride_statuses` 検索 4 種類だけで **364.039 秒（52.4%）**、**676,135,034 行**を走査しています。

インデックス追加後の比較時に、累積時間へ別のマッチング用 SQL（4.010 秒）を含めていた集計条件を修正しました。各表・CSV の個別クエリの数値に変更はありません。

| SQL の要約 | 回数 | 累積秒 | 平均 ms | p95 ms | 平均走査行数 |
| --- | ---: | ---: | ---: | ---: | ---: |
| 最新の ride_status を取得 | 70,647 | 155.596 | 2.202 | 7.673 | 4,779.6 |
| COMMIT | 67,840 | 154.531 | 2.278 | 7.589 | 0.0 |
| アプリに未通知の status を取得 | 27,825 | 84.121 | 3.023 | 10.154 | 4,788.2 |
| 椅子に未通知の status を取得 | 23,623 | 72.115 | 3.053 | 10.002 | 4,785.4 |
| owner/chairs: 座標履歴から移動距離を集計 | 160 | 56.982 | 356.137 | 679.688 | 537.7 |
| ride ごとの status 履歴を取得 | 19,273 | 52.206 | 2.709 | 9.359 | 4,783.7 |
| chair ごとの最新座標を取得 | 2,075 | 31.197 | 15.035 | 50.546 | 30,402.9 |
| chair ごとの最新 ride を取得 | 48,184 | 18.678 | 0.388 | 1.473 | 820.1 |
| access_token で chair を取得 | 48,403 | 16.761 | 0.346 | 1.161 | 531.4 |

クエリ全文と全 75 種類の集計は [slp.csv](./slp.csv)、HTTP の全 16 グループは [alp.csv](./alp.csv) に保存しています。

## 実行計画とコードで確認したこと

1. **ride_statuses の検索が全件走査になっている。** 実 DB の `SHOW INDEX` では主キー `id` のみ。最新 status、アプリ未通知、椅子未通知の各検索を `EXPLAIN` すると、いずれも `type=ALL`、`key=NULL`、`Using where; Using filesort`。slp の平均約 4,780 行の走査と整合する。実行後のテーブル件数は 5,049 行。該当箇所は [最新 status の共通関数](../../../webapp/rust/src/lib.rs)、[ユーザー通知](../../../webapp/rust/src/app_handlers.rs) の `app_get_notification`、[椅子通知](../../../webapp/rust/src/chair_handlers.rs) の `chair_get_notification`。
2. **nearby-chairs はループ内で SQL を繰り返す。** `app_get_nearby_chairs` は全椅子取得後、椅子ごとの rides、ride ごとの最新 status、椅子ごとの最新座標を順に取得している。最新座標の検索も実 DB では主キーのみで `type=ALL`、`Using where; Using filesort`。slp ではこの検索が平均 30,403 行を走査し、平均 15.0 ms。これらの反復が HTTP の平均 519 ms に寄与するという仮説が立つ。ただし HTTP と SQL の個別の紐付けは未計測。
3. **owner/chairs は履歴のウィンドウ関数と集約を毎回実行する。** [owner_get_chairs](../../../webapp/rust/src/owner_handlers.rs) は `chair_locations` に `LAG`、`SUM` を適用するクエリを 1 回実行する。HTTP と SQL はどちらも 160 回、累積時間は HTTP 59.257 秒 / SQL 56.982 秒で近く、この SQL が当該 API の主要な時間消費箇所と考えられる。個々のリクエストを対応付けた測定ではない。
4. **COMMIT も累積 154.531 秒（22.2%）を占める。** `app_get_notification` / `chair_get_notification` / 座標更新などのトランザクションを確認した。今回のログだけではディスク同期・競合・その他の内訳は特定できない。耐久性設定変更やトランザクション削除の効果は未検証。
5. **通知内にも繰り返し取得がある。** `app_get_notification` が呼ぶ `get_chair_stats` は ride 一覧を取得した後、ride ごとに status 履歴を取得する。該当する履歴取得 SQL は 19,273 回 / 52.206 秒だった。

最初の検証候補は、`ride_statuses` の検索条件と並び順に対するインデックスの検討です。次に、通知内の履歴取得回数、nearby-chairs の反復問い合わせ、owner/chairs の履歴集約を個別に計測するのが妥当と考えます。いずれも効果・スコアへの寄与は未検証です。

## 計測の限界と後処理

- alp と slp は HTTP ごとの SQL ウォーターフォールを作らない。ルートと SQL の関係は、この実装の呼び出し箇所を照合したもの。DB 接続プール待ちや Rust 内部処理の内訳は未計測。
- 全 SQL の記録と既存の Rust DEBUG ログはスコアに影響しうる。最適化を検証する際は計測条件を揃える。
- 初回の自動復元で数値変数を引用したため MySQL が拒否した。数値の復元方法を修正し、同じ実行のログを回収した。ベンチマーク自体は再実行していない。
- MySQL 設定 7 項目が計測前と一致することを確認。`slow_query_log=OFF`、`long_query_time=10`、`log_slow_extra=OFF`。Rust の接続も再起動で更新した。
- slp の CSV は SQL 内のカンマが引用されなかったため、TSV を Python 標準 CSV writer で変換した。全行で列数を確認した。
- 原本: `_tmp/profiles/20261006-001357/`。生ログは約 900 MB で Git 対象外。集計 CSV と本メモのみ保存。
- 再計測は `task profile:bench`。詳細は [計測手順](../../../development/profiling/README.md)。
