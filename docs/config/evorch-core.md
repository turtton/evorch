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

## メモリ（glibc arena 上限）

Linux/glibc の `evorch-core` は起動直後（`at_entry()` の直後。planned 再起動の exec 後も同じ入口）に
`mallopt(M_ARENA_MAX, 2)` を呼び、malloc arena を最大 2（main 含む）に制限する。履歴読み出しなど
複数スレッドで確保した後の解放済みページを再利用しやすくし、常駐メモリを抑えるため。
`MALLOC_ARENA_MAX` または `GLIBC_TUNABLES` の `glibc.malloc.arena_max` が指定されている場合
（空・不正値を含む）は何も呼ばず glibc に委ね、環境変数も書き換えない。設定に失敗した場合は
固定文言の警告を stderr に出して起動を続ける。GUI・CLI・非 glibc では何もしない。

## ライフサイクルと所有権

Unix socket は検証済みの `$XDG_RUNTIME_DIR/evorch-core-<uid>/<config-identity-hash>.sock`
を優先する。`XDG_RUNTIME_DIR` 未設定時だけ `/tmp/evorch-core-<uid>/` を使う。
設定済みの場合は絶対パス・実ディレクトリ・当該 UID 所有・0700・非 symlink を
要求し、不正・存在しない・長すぎるパスでは起動を拒否する（黙って fallback しない）。
ディレクトリ 0700、socket/lock 0600、所有者・symlink を検査する。
startup lock と、プロセス終了まで保持する flock は canonical config directory 内の
`.evorch-core.start` / `.evorch-core.lock` に置く。runtime directory の設定が異なる
起動間でも同じ DB の lifetime lock を共有する。
stale socket は lifetime lock の取得後だけ回収する。lock ファイルは unlink しない。

GUI・CLI（Unix socket）と Web の実接続がすべて 0 件で、かつ実行中作業なしの
状態が `idle_grace_secs` 継続した場合だけ自動終了する（段階 18a）。Web 配信が
有効なだけでは保持しない。HTTP keep-alive と SSE も接続の存続中は保持する。
接続・切断を観測すると猶予をリセットし、最後の切断から改めて全猶予を取る。
0 秒は即時アイドル終了。プラットフォームの時計で表現できない巨大な猶予は
期限なしとして扱い、overflow させない。

完了済み interactive turn の通常の次回チャット入力待ちは「作業なし」とする。
phase が `Waiting` であるだけでは idle とせず、turn 完了記録と未処理入力・
割り込みメッセージ・compaction の有無を確認する。active turn、tool・子 run・
lane・escalation などの active wait、shell job、background/area 作業、評価、
復旧の再試行・lane admission/停止の永続化、restart/drain は引き続き保持する。
P9（段階 06）の復旧が完了するまでは、未回答の質問（blocking でないものや
停止済み requester の質問を含む）と承認権限も保持理由とし、保存済み質問の
読み取りに失敗した場合は終了しない。18a はそれらの復旧や自動回答を追加しない。
SIGTERM/SIGINT も同じ終了経路（受付停止、接続/SSE 閉鎖、
run/checkpoint と shell の drain、永続化 flush、lease・lock 解放）を使う。
（以前の終了経路にあった owner quiesce は、ADR 0024 の owner 機構とともに PR 20（#866）で撤去された。）

> **歴史的記述（PR 20 / #866 で撤去済み）。** 次の段落の ADR 0024 の owner registry
> （`owners.db`）は撤去され、core は `owners.db` を作らず、読みも書きもしない。
> 新しい chat thread の owner 行の予約も、他 GUI 所有による `PermissionDenied` もない。
> 切り替え手順は [single-core-integration.md](../single-core-integration.md) の
> 「PR 20 以降の更新（旧 core からの切り替え）」を参照。

P7a は ADR 0024 の追加 owner。registry は GUI と同じ
`${XDG_STATE_HOME:-~/.local/state}/evorch/threads/owners.db` を使う。
新しい chat thread の未登録 owner 行だけ予約し、既存行を claim/handoff しない。
他 GUI 所有の書き込みは `PermissionDenied`。Released 行も自動取得しないため、
既存会話の編集や再起動後の所有権移譲は後続作業。

起動時に `sidebar.json` の project id/root を読み取り、現在のディレクトリも
安定した project id で公開する。projects は `ListProjects` で取得できる。
sidebar の draft・archive 状態は書き換えない。モデルのない設定ではツールを一切
登録せず、履歴・health の閲覧は可能だがモデル実行は失敗する。

## learning の durable queue

learning のジョブは durable queue（`learning_jobs`）に積まれる。admission で dedup claim、
Pending のジョブ、不変の evidence を 1 つのトランザクションで作る。状態は
Pending → Started（CAS）→ Finished / Failed / Abandoned と進み、戻らない。
core の再起動時は、同じ config の死んだ core が残した Pending のジョブだけを引き継ぐ。
GUI のジョブ、生きている別プロセスのジョブ、生死の判定できない origin のジョブは奪わない。
Started のまま残ったジョブは再実行せず、終端にして evidence を削除する。
共有 janitor は死んだ origin のジョブを清掃する。

## 出力と後続作業

Unix/Web は同じ CommandHost と Projection を使う。`HostBackend` が reply、
subscription snapshot、event を **送信前** に secret-guard で保護する。
WorkbenchBackend の protect_output を JSON 応答の最終 layer と SSE data の
シリアライズ時に呼び、adapter 合成のエラー・Ping/Pong・ResyncRequired も保護する。
request/correlation/subscription id や JSON object key も例外にしない。Unix も
handshake と最終 writer で同じ保護を適用し、早期 Subscribe reply の迂回を防ぐ。
backend を通らない decode エラーも入力値を反射しない。未知 type・未知フィールド名・
不正な値はエラー文に含めず、JSON syntax エラーは固定文言と行・列だけを返す。
runtime の入力・既送信 prompt は transport が書き換えない。
SafeRestart capability はまだ提供しない。

`CoreStatus` は Unix と Web の実接続数を `running.clients` に集計し、接続中の
`connected_clients` と Running turn の `active_turn` を `retention_reasons` に
公開する。hello・health・GetCoreStatus は同じ集計を使用する。HTTP keep-alive と
SSE は TCP 接続単位で数え、タブや session ID の重複排除はしない。実 Web owner を
引き渡さない旧 embedding API だけは、保守的な `web_serving` 保持を継続する。
`renderer_reconnect_grace` は、CLIENT preview の制御接続が切れた後の有限の再接続
猶予（30 秒）だけを表す wire 上の保持理由で、生きた client・helper の watcher・
実行中の tool ではないため `running` には数えない。この変更ではまだ誰も出さない。
`tool_calls` / `waiting_lanes` / `shell_jobs` の公開 counter と、それらの保持理由は
まだ未集計（0 / 未掲載）。実際の idle 判定は公開 status ではなく、runtime・shell・
compaction の作業状態と接続 census、切断後も残る accepted Web command / SSE
delivery の活動で行う。これらの完了通知でも猶予をリセットする。detached work の
panic / abort で正常完了を確認できない場合は、保守的に自動終了を抑止する。

TODO(P10/P11): GUI と headless の composition は今回は変更せず、bootstrap と
重複している。両者の bootstrap 利用、GUI/CLI の自動接続・起動、旧所有権撤去、
live 設定/sidebar reload、GUI trust/allowed-directory 設定の統合は後続。
P7a は shared maintenance leases と tombstone fence、確定済み削除の reclaim を
組み立てるが、GUI sidebar の archive 判定・移行や新規 retention 削除確定はしない。
durable lane の自動復旧と安全点再起動・自動再開も後続であり、GUI を置き換えない。

## スレッド保守と idle

~~~toml
[storage.thread_retention]
tool_body_delete_after_days = 7 # 0 は Tool 本文の除去だけを無効化
~~~

Tool の長い表示用本文は、canonical thread（root と子 run 全体）に最後の活動から
7 日以上更新がないと [removed by retention] へ置換される。archive の有無は問わず、
ツール名・呼び出し ID・成否などの概要は残る。既定 7 日は既存の自動 archive とそろえた値。
1 週間以上更新のない本文の参照は少なく、必要な thread は pin や eval sample で守れる。
実行中・入力待ち・未確定 admission・未回答質問・attached/summary の open lane・
pin・eval sample は保護される（detached lane や未完了 goal だけでは保護しない）。
provider 用の run_messages・run_contexts・checkpoint は変更せず、共有 blob も残るため、
完全な情報消去ではない。参照を外した後の GC と online incremental vacuum は独立して
進む。[予算・所要時間・停止中の full VACUUM](../storage-maintenance.md#tool-本文の保持期限とオンライン回収) を参照。

[storage.thread_retention] の auto_archive_after_days と archived_delete_after_days はそれぞれ既定 7 日、独立した 0 で新規処理を停止する。削除の既定 30→7 日化により、次回の起動時排他フェーズで 7〜30 日前の既存 archive が削除対象になり得る。30 の明示設定は維持される。

PR1 は通常 core（単一 core でも）へ auto-archive authority を渡さず、`AuthorityUnavailable` で自動 archive・添付整理・scratch hint を保留する（この間は DB・sidebar・添付を開かず lock も取らない）。PR2 で配線される。判定は派生状態を持たず、1 tick・1 family を正本から直接読む。詳細は [ストレージ保守](../storage-maintenance.md#thread-自動-archivepr1-正本判定と保留中の実装)。

保守 backlog・将来の期限は busy() に加えない。保守だけのために idle 終了を抑止せず、停止中の別 daemon もない。次回稼働で再開する。shutdown／planned exit は retention producer を止め、実行中の有界処理の ACK と worker join の後に bridge flush／storage barrier を行う。barrier 後の retention commit は許さない。
