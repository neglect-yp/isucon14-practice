# Rust 初期実装のベンチマーク

Rust の初期実装を性能改善せずに 1 回計測した結果、**PASS、スコア 8,715** でした。今後、このローカル環境で同じ条件の計測を比較するための基準値です。Go 実装でのベンチマークは実行していません。

## 結果

| 項目 | 値 |
| --- | --- |
| 実行開始 | 2026-10-04 18:14:46 JST |
| 実行終了 | 2026-10-04 18:15:55 JST |
| 負荷走行時間 | 60 秒 |
| コマンド全体の所要時間 | 69.122 秒。起動、初期化、検証を含む |
| 判定 | `pass=true` |
| スコア | 8,715 |
| コマンド終了コード | 0 |
| 種別エラー数 | `map[26:1]` |

標準出力の最終結果は次のとおりです。

```text
time=18:15:55.405 level=INFO msg=結果 pass=true スコア=8715 種別エラー数=map[26:1]
```

18:15:16 に、オーナーの椅子一覧の `total_distance` の反映が遅いという警告 `CODE=26` が 1 件記録されました。ベンチマーカーの最終判定は PASS です。原因の調査や性能改善はまだ行っていません。

## ソースと起動設定

基準コミットは `71fa6f1b4d38cb6ce70a5bbaca3ea0a1ed94c3c1` です。計測時の `git diff HEAD -- webapp bench` は空で、Rust のソース、SQL、ベンチマーカー、Cargo.lock は変更していません。

実装を切り替えるため、次の起動設定を追加しました。

- ルートの `Taskfile.yml` に `rust:build` と `rust:run` を追加。
- `development/compose-rust-local.yml` を追加し、既存の `compose-local.yml` と組み合わせて Rust コンテナを起動。
- マッチング用コンテナの接続先を `http://webapp:8080/api/internal/matching` に変更。呼び出し間隔は既存と同じ 0.5 秒。
- ホストで動いていた Go API を停止し、同じ 8080 番ポートを Rust API に切り替え。

MySQL コンテナは作り直していません。ベンチマーカーによる `/api/initialize` で初期データを読み込みました。既存の `frontend/mise.local.toml` は、前のフロントエンド起動作業で追加したものです。

## 実行環境

| 項目 | 条件 |
| --- | --- |
| ホスト | macOS 27.0、arm64、Mac16,8、論理 CPU 12、メモリ 24 GiB |
| Docker | Colima、Linux aarch64、VM の CPU 4、Docker が報告するメモリ 8,309,280,768 bytes |
| Rust | rustc 1.83.0、cargo 1.83.0、コンテナ内で release ビルド |
| ビルド定義 | 既存の `development/dockerfiles/Dockerfile.rust`。依存関係は `--locked`、アプリのビルドは `--locked --frozen` |
| API | Rust コンテナ、ホストの `http://localhost:8080` に公開 |
| DB | 既存の MySQL 8.4.11 コンテナ |
| アプリのログ設定 | 既定値 `info,tower_http=debug,axum::rejection=trace` |
| ベンチマーカー | ホスト上の Go 1.25.3 darwin/arm64、Task 3.54.0 |
| 決済サーバー | ベンチマーカー内蔵サーバー。ホストの 12346 番ポート、アプリからは `http://host.docker.internal:12346` |
| フロントエンド | Vite 開発サーバーを `localhost:3000` で起動したまま計測 |

Rust と DB は同じ Colima VM 上で動作し、ベンチマーカーも同じ Mac のリソースを利用します。コンテナ個別の CPU・メモリ制限は設定していません。イメージ ID、コンテナ ID、コンパイラーの詳細は [environment.json](environment.json) に保存しています。

### Colima の設定値

2026-10-05 JST に、`~/.colima/default/colima.yaml`、`colima list --json`、`colima status` で確認しました。ベンチマーク翌日の確認値として追記しています。

| 項目 | 設定値 |
| --- | --- |
| Colima バージョンとプロファイル | 0.9.1、`default` |
| CPU | 4 vCPU |
| メモリ | 8 GiB（8,589,934,592 bytes） |
| ディスク容量 | 100 GiB（107,374,182,400 bytes、使用量ではなく割当容量） |
| アーキテクチャ | `aarch64` |
| コンテナランタイム | `docker` |
| 仮想化方式 | `vz`（macOS Virtualization.Framework） |
| マウント方式 | `virtiofs` |
| ポート転送方式 | `ssh` |
| Rosetta | `false` |

メモリの設定値 8 GiB と、Docker が報告する 8,309,280,768 bytes は区別して記録しています。確認日時と設定値は [environment.json](environment.json) の `colima` にも保存しました。Colima の設定自体は変更していません。

### 比較時の条件

API のみを公開する構成のため、`--skip-static-sanity-check` を指定しました。静的ファイルの検証に加えて、負荷走行中の静的ファイル取得も省略されます。この結果を静的ファイル取得を含む構成や競技環境のスコアと直接比較しないでください。1 回だけの計測で、繰り返し実行によるばらつきは未確認です。

## 再実行

Go API などが 8080 番ポートを使用していない状態で、リポジトリのルートから起動します。

```sh
task rust:run
```

次に、以下のコマンドを実行します。データベースは初期化されるため、計測中はブラウザ操作を控えてください。

```sh
cd bench
task run-local -- --payment-bind-port 12346 --payment-url http://host.docker.internal:12346 --skip-static-sanity-check --fail-on-error
```

`bench/Taskfile.yml` の `run-local` が `--target http://localhost:8080 -t 60` を指定します。ホストの 12345 番ポートは既存の決済モックが使用しているため、ベンチマーカーには 12346 を指定しました。

今回の計測後、Rust コンテナは起動したままにし、DB の `settings.payment_gateway_url` を `http://paymentmock:12345` に戻しました。ベンチマークが生成したデータは残しています。以降のベンチマークでは初期化処理が決済 URL を再設定します。

## 保存した記録

- [標準出力](attempt-01.stdout.log)：スコア、最終判定、警告、地域・オーナー別の最終情報。
- [標準エラー出力](attempt-01.stderr.log)：実行コマンドとベンチマーカーの詳細ログ。
- [実行メタデータ](attempt-01.json)：コマンド、開始・終了時刻、終了コード、ソース差分。
- [環境情報](environment.json)：ソフトウェアのバージョン、コンテナとイメージの識別情報。

以下の生成ファイルは同じディレクトリにローカル保存し、Git の管理対象から除外しています。上記の計測結果とメタデータはコミット対象です。

- `webapp.log.gz`、`db.log.gz`、`matcher.log.gz`：計測開始から終了までの詳細ログ。DB とマッチング用コンテナには当該期間の出力なし。
- `build.log.gz`、`startup.log.gz`：ビルド・起動ログ。
- `setup.patch`：計測時点の起動設定と手順の差分。実際の設定ファイルと手順は Git で管理。

ローカルの圧縮ログは `gzip -dc webapp.log.gz` などで読めます。
