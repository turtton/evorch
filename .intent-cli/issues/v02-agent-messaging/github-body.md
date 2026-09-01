## Goal

AgentRun の lifecycle 完了通知とは独立した durable AgentMessage channel を実装する。現行 `send_message` を `send`（fire-and-forget）/ `wait_reply`（correlation + timeout）/ `inbox`（未読 pull）へ再編し、親子限定 addressing を強制する。Running recipient には親 sender の steering / 非親 sender の aside、Waiting / idle recipient には wake として配送し、send / reply / steering を Event Bus と SQLite transcript に永続化する。run crash 時は親が新規 run を起動して transcript から文脈を再構成する。

## Why This Slice Exists Now

v0.2 の goal + contract → implement → PR → review 往復を harness 内で回すには、worker の質問・blocked 理由・途中成果を完了前に Orchestrator へ返す mid-run relay が必要である。現行 `AgentRuntime::send_message` は文字列を容量 32 の mpsc inbox に `try_send` するだけで、meta-op 側は直後に宛先 run の Done/Error まで待つ。sender / recipient / message id / reply correlation / 配送種別は Event Bus や storage に残らず、message channel と lifecycle completion が実質結合している。agent-runtime-kernel / orchestration overview で send / wait_reply / inbox、steering / aside / wake、親子 relay は確定済みで、grill `grill-v02-loop-foundation` Q2 により transcript 永続化、crash 時の新規 run 再起動、strict revive の v0.3 延期まで確定した。

## Current Observed State

- `AgentRuntime::delegate_background`（`crates/runtime/src/runtime.rs`）は run ごとに `mpsc::channel(INBOX_CAPACITY)` を作り、`INBOX_CAPACITY` は 32。inbox payload は `String` のみ
- `AgentRuntime::send_message` は対象 inbox へ `try_send(text)` し、unknown / terminated run を error にするが、sender identity・message identity・reply correlation・kind を持たない
- `meta::send_message`（`crates/runtime/src/meta.rs`）は enqueue 成功後に `wait_for_run` を呼び、親 run を Waiting にして宛先の Done/Error まで待つ。fire-and-forget と reply 待機が分離されていない
- `agent_loop::wait_for_input` は interactive run が Waiting の時だけ inbox を受け取り、user message として context へ追加して Running に戻す。Running 中 steering / aside の注入経路はない
- `AgentRunPhase` は Pending / Running / Waiting / Done / Error の5相で、parked はない
- `LifecycleEvent` は `AgentRunStateChanged`、`BackgroundTaskStarted` / `Completed` / `Cancelled` 等を持つが、AgentMessage 専用 event はない
- storage は SQLite event sourcing（ADR 0018）を実装済みで、`MessageRecord` と `Database::message` / `messages_by_session`、event append / `events_by_session` がある。AgentMessage transcript の既存接続先はあるが runtime messaging と未接続
- `RoleCapabilities` は `allowed_tools` / `network` / `can_delegate` を持ち、現行 Orchestrator tool set に `send_message` / `wait` がある。親子限定 tree addressing の設計根拠は ADR 0022

## Accepted Baseline You May Assume

- agent-runtime-kernel overview v0.2 計画: messaging op は `send` / `wait_reply` / `inbox`、Running recipient は steering / aside、Waiting / idle recipient は wake、AgentMessage と completion channel は分離する
- orchestration overview: mid-run relay は worker の完了前通知・質問・blocked 理由を親 Orchestrator に中継し、sibling 間は親が relay する
- ADR 0022: addressing は parent↔child に限定し、nested delegation でも tree topology を守る。sibling 直接通信はしない
- ADR 0018: SQLite event sourcing を durable state の土台とし、event / transcript を監査・復元可能にする
- grill `grill-v02-loop-foundation` Q2: AgentMessage（send / reply / steering）を transcript 永続化する。crash 時は親が新規 run を起動して transcript から文脈再構成。会話・tool snapshot の strict revive は v0.3
- providers の `Message { role, content }` はモデル会話型であり、agent 間配送 envelope とは分離する

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/runtime/`, `crates/event-bus/`, `crates/storage/`

Target part: lifecycle completion と独立した AgentMessage channel、send / wait_reply / inbox、親子 addressing、steering / aside / wake、Event Bus emit と transcript persistence

## In Scope

- AgentMessage envelope（message_id / sender_run_id / recipient_run_id / kind: send|reply|steering / content / optional reply_to）の型定義。provider `Message` とは分離
- meta-op `send` / `wait_reply` / `inbox`。send は fire-and-forget、wait_reply は message correlation + timeout、inbox は未読 pull
- 現行 `send_message` の互換処理。残す場合も run 終端待ちの意味を廃止し、新 API への alias または migration error に限定
- ADR 0022 の親子限定 addressing を共通配送入口で fail-closed 強制（parent↔child 許可、sibling / 無関係 / 自己宛拒否）
- Running / busy recipient: 親 sender は steering、非親 sender は aside として step boundary まで保留
- Waiting / idle recipient: context へ message を追加して wake、Running へ遷移
- AgentMessage 専用 EventKind。`BackgroundTaskCompleted` / `Cancelled` と channel / payload を分離
- Event Bus → storage ingress → SQLite transcript の永続化。message identity・sender / recipient・reply correlation・ordering の復元
- Error / crash 後に transcript を読み出し、別 RunId の新規 run へ再構成 context を渡す recovery seam
- 既存 delegate_background / delegate / wait / cancel / list_agents / inspect_agent / GUI event consumer の回帰維持

## Out Of Scope

- parked run の会話・tool 状態・in-flight provider request の snapshot 復元 — v0.3 strict revive
- 同一 RunId / Tokio task identity の復活。v0.2 は新規 run + transcript 再構成のみ
- sibling 直接通信、broadcast、任意 topology、distributed broker / remote worker transport
- durable exactly-once delivery / cross-process federation
- goal state / finish gate / continuation — `v02-orchestrator-loop`
- context compaction — `v02-context-compaction`
- worktree / branch / merge — `v02-workspace-isolation`
- GUI transcript pane / Agents dashboard — `v02-gui-workbench-restructure`

## Standalone Child Issue Contract

`turtton/evorch` の `crates/runtime/`・`crates/event-bus/`・`crates/storage/` に、run lifecycle completion と独立した durable AgentMessage channel を実装する。AgentMessage は message_id / sender_run_id / recipient_run_id / kind（send / reply / steering）/ content / optional reply_to を型付きで保持し、provider 会話 `Message` とは分離する。meta-op は `send`（enqueue + emit 後ただちに返る fire-and-forget）/ `wait_reply`（message correlation + timeout）/ `inbox`（未読を配送順に一度だけ pull）とする。ADR 0022 の parent↔child addressing を全入口で強制し、sibling・無関係 run・自己宛は拒否する。Running recipient では親 sender を steering、非親 sender を aside として step boundary で注入し、Waiting / idle recipient は wake して Running に戻す。AgentMessage は専用 EventKind として emit し、`BackgroundTaskCompleted` / `Cancelled` と分離したまま既存 SQLite storage ingress を通じて transcript 永続化する。sender / recipient / reply correlation / ordering を read API で復元可能にし、crash 時は親が別 RunId の新規 run を起動して transcript から文脈再構成できる recovery test を追加する。同一 run の snapshot revive、sibling 直接通信、goal/finish continuation、workspace isolation、GUI pane は実装しない。PR は `main` をターゲットにする。

## Acceptance Criteria

- AgentMessage envelope が message_id / sender_run_id / recipient_run_id / kind / content / reply_to を保持し、Event Bus event として serialization round-trip する unit test がある
- `send` が宛先 run の Done/Error を待たず返ることを async test で検証する
- `wait_reply` が対応 reply のみ返し、無関係 message を消費せず、timeout を型付き error で返す test がある
- `inbox` が未読を配送順に返し、既読を再返却しない test がある
- parent↔child は許可、sibling / 無関係 / 自己宛は send / reply / steering の全入口で fail-closed 拒否される matrix test がある
- Running recipient への parent steering、non-parent aside の step-boundary 注入、Waiting / idle recipient の wake → Running を integration test で検証する
- AgentMessage が Event Bus から SQLite transcript へ永続化され、read API で ordering・sender / recipient・reply correlation を復元できる integration test がある
- message 到着だけでは `BackgroundTaskCompleted` / `Cancelled` を emit しない回帰 test がある
- Error run の transcript から別 RunId の新規 run へ再構成 context を渡せる recovery test がある。同一 run / tool state の strict revive は実装しない
- 既存 runtime meta-op と GUI event consumer の回帰 test が pass する
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check` が pass する

## Verification

- AgentMessage / EventKind serialization unit test
- send fire-and-forget、wait_reply correlation + timeout、inbox unread ordering の async unit test
- parent-child addressing matrix test（全 message kind / 全入口）
- steering / aside / wake の runtime integration test
- Event Bus → storage → transcript read round-trip integration test（secret guard / hard limit 経路込み）
- crash → transcript read → new RunId context reconstruction test
- lifecycle completion channel 分離と既存 delegate / wait / cancel / GUI consumer の回帰確認
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Related Links

- intents/evorch/features/agent-runtime-kernel/overview.md（v0.2 messaging op / 配送语义 / scope 修正）
- intents/evorch/features/orchestration/overview.md（mid-run relay / 親子 messaging）
- intents/evorch/decisions/0018-sqlite-event-sourcing.md
- intents/evorch/decisions/0022-parent-child-tree-addressing-and-nested-delegation.md
- intents/evorch/interviews/grill-v02-loop-foundation.json（Q2）
- 後続 slice: `v02-context-compaction`、`v02-gui-workbench-restructure`、`v02-orchestrator-loop`

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `features/agent-runtime-kernel/overview.md` primary。supporting: orchestration overview、ADR 0018 / 0022、grill record。新規 intent は不要
- ADR candidate: none（durability と addressing は既存 ADR、strict revive 延期は feature scope 修正として確定済み）
- Diagram candidate: none
- Docs update: none（GUI / end-user surface は追加しない）
- Closeout writeback expected: yes。AgentMessage envelope / EventKind、reply waiter、steering / aside / wake、storage schema、crash recovery seam を overview に記録する

## Guide Reachability (G645)

While the author still knows the answer, name the guide surface and role that route to every
role-facing surface this slice adds, or explicitly say that no role-facing surface is added. A
blank answer is not treated as no-surface. The closeout record is a debt check, not a merge gate.

この slice は Orchestrator が利用する role-facing meta-op surface（`send` / `wait_reply` / `inbox`）を追加する。route: orchestration overview の委譲ループ protocol（mid-run relay）→ Orchestrator → AgentMessage meta-op / transcript。`no_role_facing_surface: false` を宣言する。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
