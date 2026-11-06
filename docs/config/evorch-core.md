# evorch-core（ADR 0045 P7a）

```sh
evorch-core --config-dir ~/.config/evorch --web --foreground
curl http://127.0.0.1:7878/api/v1/health
```

`--config-dir` は設定・credentials・`evorch-events.db`・artifacts のディレクトリ。
省略時は既存のユーザー設定ディレクトリ。P7a は常に foreground で動作し、
daemon 化や GUI からの自動起動はしない。readiness は Unix/Web listener の
bind 後、stdout に JSON 一行（`status=ready`, `socket`, `core`, `web`）で通知する。
同じ canonical config directory の起動中・稼働中プロセスがあれば終了コード 2、
その他の起動エラーは 1。PID ファイルや socket の存在は生存判定に使わない。

ユーザー層 `config.toml`:

```toml
[core]
idle_grace_secs = 60

[web]
enabled = false
port = 7878
allowed_hosts = ["my-host.my-tailnet.ts.net"]
allowed_origins = []
```

Web は明示的に有効化したときだけ `127.0.0.1` に bind する。`--web` は今回の
起動だけ `enabled=true` にする。プロジェクト層から Web を有効化できない。
ローカルの loopback Host と同一 Origin は既定で許可される。

スマホからの接続例:

```sh
tailscale serve --bg http://127.0.0.1:7878
```

Serve が表示する実際のホスト名（scheme・port なし）を `allowed_hosts` に登録する。
同一オリジンの Serve には `allowed_origins` の追加は不要。別 frontend を置く場合
だけ `https://frontend.example` のような完全な Origin を追加する。独自認証はないため、
Tailscale の認証・ACL を信頼境界とし、公開リバースプロキシで露出しない。
この PR の UI は既存 adapter の assets を使う。スマホ UI の P6 は別 PR。

## ライフサイクルと所有権

Unix socket は `/tmp/evorch-core-<uid>/<config-identity-hash>.sock`。
ディレクトリ 0700、socket/lock 0600、所有者・symlink を検査する。
startup lock と、プロセス終了まで保持する flock を使う。
stale socket は lifetime lock の取得後だけ回収する。lock ファイルは unlink しない。

クライアント 0・実行中作業なし・Web 無効が猶予中継続した場合だけ自動終了する。
**Web 配信有効中は、接続がなくてもアイドル終了しない。**
0 秒は即時アイドル終了。プラットフォームの時計で表現できない巨大な猶予は
期限なしとして扱い、overflow させない。
P7a では非終端の run（ユーザー入力・質問・承認待ちの `Waiting` を含む）と
shell job・保存済み chat の compaction を保持理由として扱う。
SIGTERM/SIGINT も同じ終了経路（受付停止、接続/SSE 閉鎖、owner quiesce、
run/checkpoint と shell の drain、永続化 flush、lease・lock 解放）を使う。

P7a は ADR 0024 の追加 owner。registry は GUI と同じ
`${XDG_STATE_HOME:-~/.local/state}/evorch/threads/owners.db` を使う。
新しい chat thread の未登録 owner 行だけ予約し、既存行を claim/handoff しない。
他 GUI 所有の書き込みは `PermissionDenied`。Released 行も自動取得しないため、
既存会話の編集や再起動後の所有権移譲は後続作業。

起動時に `sidebar.json` の project id/root を読み取り、現在のディレクトリも
安定した project id で公開する。projects は `ListProjects` で取得できる。
sidebar の draft・archive 状態は書き換えない。モデルのない設定ではツールを一切
登録せず、履歴・health の閲覧は可能だがモデル実行は失敗する。

## 出力と後続作業

Unix/Web は同じ CommandHost と Projection を使う。`HostBackend` が reply、
subscription snapshot、event を **送信前** に secret-guard で保護する。
runtime の入力・既送信 prompt は transport が書き換えない。
SafeRestart capability はまだ提供しない。

TODO(P10/P11): GUI と headless の composition は今回は変更せず、bootstrap と
重複している。両者の bootstrap 利用、GUI/CLI の自動接続・起動、旧所有権撤去、
live 設定/sidebar reload、GUI trust/allowed-directory 設定の統合は後続。
P7a は shared maintenance leases と tombstone fence、確定済み削除の reclaim を
組み立てるが、GUI sidebar の archive 判定・移行や新規 retention 削除確定はしない。
durable lane の自動復旧と安全点再起動・自動再開も後続であり、GUI を置き換えない。
