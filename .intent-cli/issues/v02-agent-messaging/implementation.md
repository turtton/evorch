# v02-agent-messaging Implementation Packet

## Goal

`crates/runtime/`・`crates/event-bus/`・`crates/storage/` に、AgentRun の lifecycle 完了通知とは独立した AgentMessage channel を実装する。現行 `send_message` を `send`（fire-and-forget）/ `wait_reply`（message correlation + timeout）/ `inbox`（未読 pull）へ再編し、親子限定 addressing（ADR 0022）を全入口で強制する。実行中 run には sender が親なら steering、親以外なら aside、Waiting / idle run には wake として配送する。send / reply / steering は Event Bus に emit して既存 SQLite message/event storage へ transcript として永続化し、crash 時は親が新規 run を起動して transcript から文脈を再構成できるようにする。

## Why

v0.2 の成功条件である goal + contract → implement → PR → review 往復を harness 内で自律回転させるには、worker の質問・blocked 理由・途中成果を完了前に親へ返す durable な mid-run relay が必要である。現行 `AgentRuntime::send_message` は文字列を mpsc inbox に `try_send` するだけで、meta-op 側は送信直後に宛先 run の終端まで待つため、fire-and-forget と reply 待機が分離されていない。sender / recipient / message id / reply correlation / 配送種別も Event Bus や storage に残らず、`BackgroundTaskCompleted` が事実上の完了 channel になっている。agent-runtime-kernel overview と orchestration overview で messaging 再編・steering / aside / wake・親子 relay は既に確定済みで、grill `grill-v02-loop-foundation` Q2 により AgentMessage transcript 永続化、crash 時の新規 run 再起動、厳密 revive の v0.3 延期まで確定した。本 packet はその計画を実装へ落とす基盤 slice である。

## Scope

- provider 会話 `Message` と分離した AgentMessage envelope を runtime / event-bus 境界に定義する。最低限 `message_id` / `sender_run_id` / `recipient_run_id` / `kind`（send / reply / steering）/ `content` / 任意 `reply_to` を型付きで保持する
- meta-op を `send` / `wait_reply` / `inbox` に再編する。`send` は enqueue と Event Bus emit が成功した時点で返り、宛先 run の完了を待たない。`wait_reply` は指定 message への reply のみを timeout 付きで待つ。`inbox` は未読を配送順に返す
- 既存 `send_message` の互換扱いを実装 slice で明示する。残す場合も同期的に run 完了を待つ意味は廃止し、新 API への薄い alias または明確な migration error に限定する
- ADR 0022 の親子限定 addressing を runtime の共通配送入口で強制する。parent↔child のみ許可し、sibling / 無関係 run / 自己宛を fail-closed で拒否する。nested delegation でも同じ tree 検証を使う
- recipient が Running / busy の場合、sender が recipient の親なら進行中 turn へ steering 注入する。親でなければ aside queue に保持し、tool result 等の step boundary で次の user/synthetic message として注入する
- recipient が Waiting / idle の場合、message を context に追加して run を wake し Running へ戻す。message 到着と lifecycle completion は独立させる
- AgentMessage の send / reply / steering 配送を専用 EventKind として emit する。`BackgroundTaskStarted` / `Completed` / `Cancelled` と同じ payload に押し込まず、GUI / diagnostics が message と lifecycle を別々に購読できる契約にする
- 既存 storage の Event append と `MessageRecord` / session 別 message read API を使い、AgentMessage を transcript に永続化する。message identity・sender / recipient・reply correlation・配送順を復元可能にする。schema 拡張が必要な場合は migration と round-trip test を含める
- crash / Error 後は親が別 RunId の新規 run を作り、永続化 transcript から必要な会話を読み出して再構成用 prompt/context として渡せる recovery seam を実装・検証する
- 既存 `delegate_background` / `delegate` / `wait` / `cancel` / `list_agents` / `inspect_agent` と Event Bus consumer の下位互換を維持する

## Out of scope

- parked run の会話・tool 状態・in-flight provider request を snapshot から復元する厳密 revive — v0.3
- 同一 RunId / 同一 Tokio task identity を crash 後に復活させる state restore — v0.3。v0.2 は親による新規 run 再起動 + transcript 再構成
- sibling agent 同士の直接通信、broadcast、任意 topology。sibling 連携は親 Orchestrator が relay する（ADR 0022）
- durable exactly-once delivery、分散 message broker、cross-process federation、remote worker transport
- goal state / finish gate / continuation dispatcher — `v02-orchestrator-loop`
- context window 自動圧縮 / DCP — `v02-context-compaction`
- workspace worktree の作成・branch・merge — `v02-workspace-isolation`
- GUI の複数 transcript pane / Agents dashboard — `v02-gui-workbench-restructure`（本 slice は購読可能な event / storage surface を提供する）

## Verification

- unit test: AgentMessage envelope と EventKind の serialize / deserialize、message id・sender / recipient・kind・reply_to の保持
- async unit test: `send` が宛先 run の終端を待たず返ること、`wait_reply` が対応 reply のみ返して timeout / 無関係 message を正しく扱うこと、`inbox` が未読を配送順に一度だけ返すこと
- matrix test: parent↔child 許可、sibling / 無関係 / 自己宛拒否。send / reply / steering の全入口で同じ addressing checker が適用されること
- runtime integration test: Running recipient への parent steering、non-parent aside の step-boundary 注入、Waiting recipient の wake → Running 遷移
- storage integration test: Event Bus emit → SQLite 永続化 → transcript read の round-trip。sender / recipient / reply correlation / ordering が復元でき、secret guard / hard limit を bypass しないこと
- recovery test: Error run の transcript を読み、別 RunId の新規 run へ再構成 context を渡せること。同一 run の snapshot revive を行わないこと
- regression test: AgentMessage 到着だけでは `BackgroundTaskCompleted` / `Cancelled` が emit されず、既存 delegate / wait / cancel と GUI event consumer が壊れないこと
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `features/agent-runtime-kernel/overview.md` を primary とし、messaging op 再編・配送语义・transcript 永続化・crash recovery の実装確定を反映する。`features/orchestration/overview.md` の mid-run relay を supporting とする。新規 intent は不要
- ADR candidate: decline — durable event sourcing は ADR 0018、親子 addressing は ADR 0022 で既決。今回の Q2 は feature overview の v0.2 scope 修正として確定済み
- Diagram candidate: decline — parent / child channel と steering / aside / wake の flow は feature overview の記述で十分。実装で新しい topology decision が生じた場合のみ follow-up を起票する
- Docs update: decline — GUI / end-user surface は本 slice では追加しない。Orchestrator が使う meta-op route は Guide Reachability で宣言する
- Closeout learning: AgentMessage の最終 envelope / EventKind、reply waiter、addressing 強制位置、step boundary、storage schema、crash recovery seam を agent-runtime-kernel overview に記録し、orchestration overview の mid-run relay と接続する。`write_back_required: true`

- Guide reachability (G645): Orchestrator が利用する role-facing meta-op surface を追加するため `no_role_facing_surface: false`。route は orchestration overview の委譲ループ protocol（mid-run relay）→ Orchestrator → `send` / `wait_reply` / `inbox` と AgentMessage transcript

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.
