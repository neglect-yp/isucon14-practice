# ローカル計測の SSH 転送障害と回避

2026-10-06 の #3 計測中、HTTP と Docker ソケットを転送する macOS の SSH クライアントが終了した。VM とアプリケーションは稼働していたが、ベンチマーカーから `localhost:8080` への接続が拒否され、FAIL になった。

- フォアグラウンドでの再現: 終了コード 255、`poll: Invalid argument`。
- SSH ログ: 1,024 付近のチャネルで `open failed`。
- VM の転送用 sshd の open files 上限は 1,024。専用プロセスを 16,384 にしても macOS クライアントの終了は再現した。
- 一時的な Go SSH 転送クライアントに切り替えたところ、1,400 本の HTTP 同時接続と同時の Docker exec は PASS。根本原因を SSH 実装の特定箇所まで確定したわけではない。

実行した転送プログラムと依存バージョンをこのディレクトリに保存した。VM のホスト公開鍵を固定して検証し、既存の Colima 用秘密鍵をローカルで読む。秘密鍵はコピー・記録しない。専用 SSH セッションの open files 上限のみ引き上げ、VM 全体の設定や既存セッションは変えない。TCP / Unix ソケットの half-close を維持する。

再現する場合は `colima ssh-config` の Hostname、Port、User、IdentityFile を引数に使う。VM の `/etc/ssh/ssh_host_ed25519_key.pub` を `colima ssh` で取得してローカルファイルに保存する。TCP 8080 / 3306 / 12345 と Docker ソケットを使う既存転送が停止していることを `lsof` で確認する。接続を所有するプロセスがある状態でソケットを削除しない。

```text
go build -o /tmp/colima-forward .
/tmp/colima-forward SSH_HOST:SSH_PORT USER PRIVATE_KEY_PATH HOST_PUBLIC_KEY_PATH DOCKER_SOCKET_PATH
```

このプロセスの稼働中に各ポートと Docker ソケットを転送する。停止すると転送も停止する。常用サービスへの組み込みや Colima の起動設定の変更は行っていない。実際の計測では `_tmp/eight-forward-bin` を使用した。

ベンチマーカーはホストで実行し、アプリと DB は従来どおり Colima 4 CPU / 8 GiB 内。ベンチマーカーのソース、負荷条件、アプリの公開 URL は変更しない。転送実装の差を改善効果に混ぜないため、#3 以降は旧・新とも同じ転送経路で比較する。#1・#2 のスコアとの直接比較にはこの条件差がある。

除外した実行は `20261006-212601`、`20261006-213113`、`20261006-213352`、`20261006-213728`。MySQL の計測前 7 設定は復元済み。`20261006-214118` は転送実装の half-close 修正前の試行で、負荷走行前に中断した。成功計測の集計には含めない。

## 作業終了時の接続復旧

計測に使った実装は `2961c64d` の `main.go`（TCP 8080 と Docker ソケットの転送）。最終確認で、通常の Compose で公開している MySQL 3306 / 決済モック 12345 がホストから接続できないことが分かったため、この 2 ポートも追加した。性能比較の終了後の変更であり、計測結果には混ぜていない。

終了時点では `_tmp/eight-forward-final` を独立したバックグラウンドプロセスとして起動している。PID とログは `_tmp/eight-forward-final.pid` / `_tmp/eight-forward-final.log`。Colima の通常の転送へ戻すときは、このプロセスがポートとソケットを使用している点に注意する。常用の起動設定や他のコンテナは変更していない。

復旧後に Docker CLI、アプリの HTTP 401（未認証での正常応答）、MySQL の接続ハンドシェイク、決済モックの HTTP 応答を確認した。
