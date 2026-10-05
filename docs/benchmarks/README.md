# ベンチマーク記録

各計測は結果一覧に 1 行ずつ追記します。環境と実行条件は以下の共通設定を使い、個別の詳細ログやメタデータは原則として保存しません。調査で必要になった内容だけをメモに残します。

## 結果一覧

| 実行日時 JST | 対象コミット | 変更内容 | 判定 | スコア | メモ |
| --- | --- | --- | --- | ---: | --- |
| 2026-10-04 18:14 | `71fa6f1b` | Rust 初期実装 | PASS | 8,715 | `total_distance` の反映遅延の警告 `CODE=26` が 1 件 |
| 2026-10-06 00:14 | `92eaa9f1` + 計測設定 | nginx 追加、alp / slp、全 SQL ログ有効 | PASS | 8,524 | `CODE=26` が 1 件。[分析結果](./2026-10-06-alp-slp/README.md)。計測負荷があるため初回スコアとは直接比較しない |
| 2026-10-06 00:32 | `92eaa9f1` + 計測設定・インデックス | ride_statuses / chair_locations に複合インデックスを追加 | PASS | 10,245 | 警告なし。同じ計測条件の前回比 +20.2%。[比較結果](./2026-10-06-indexes/README.md) |

初回計測ではアプリケーション、SQL、ベンチマーカーのソースに変更なし。Rust の起動設定のみ追加した状態で 1 回計測しました。

## 共通環境

Colima の設定は全計測で固定します。設定値は 2026-10-05 に `~/.colima/default/colima.yaml`、`colima list --json`、`colima status` で確認しました。

| 項目 | 設定 |
| --- | --- |
| ホスト | macOS 27.0、arm64、Mac16,8、論理 CPU 12、メモリ 24 GiB |
| Colima | 0.9.1、プロファイル `default` |
| VM の割当 | 4 vCPU、メモリ 8 GiB、ディスク 100 GiB |
| アーキテクチャとランタイム | `aarch64`、`docker` |
| 仮想化とマウント | `vz`、`virtiofs` |
| ポート転送と Rosetta | `ssh`、Rosetta 無効 |
| Rust | rustc / cargo 1.83.0、既存 Dockerfile による release ビルド、依存関係固定 |
| DB | MySQL 8.4.11 |
| ベンチマーカー | ホスト上の Go 1.25.3、Task 3.54.0 |

Rust と MySQL は同じ Colima VM 内で実行し、コンテナごとの CPU・メモリ制限は設けません。ベンチマーカーも同じ Mac のリソースを利用します。

## 共通の計測条件

- 負荷走行は 60 秒、対象は `http://localhost:8080`。
- 静的ファイルの検証・取得は `--skip-static-sanity-check` で省略。
- 決済サーバーはベンチマーカー内蔵のものをホストの 12346 番ポートで起動。Rust からの接続先は `http://host.docker.internal:12346`。
- アプリのログ設定は既定値 `info,tower_http=debug,axum::rejection=trace`。
- フロントエンドの Vite 開発サーバーは `localhost:3000` で起動したままにし、計測中はブラウザ操作を控える。

静的ファイルの取得を含む構成や競技環境のスコアとは直接比較しません。アプリの設定など共通条件と異なる変更を試した場合は、結果一覧の変更内容・メモに記載します。

## 実行方法

リポジトリのルートで実行します。ベンチマークはデータベースを初期化します。

```sh
task rust:run
cd bench
task run-local -- --payment-bind-port 12346 --payment-url http://host.docker.internal:12346 --skip-static-sanity-check --fail-on-error
```
