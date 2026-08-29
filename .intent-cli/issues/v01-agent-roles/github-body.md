## Goal

ADR 0002 の capability boundary を持つ4 role（Orchestrator / Explorer / Worker / Reviewer）を実行する runtime を実装する。`AgentRun` を event-sourced な状態遷移（pending / running / waiting / done / error）を伴う Tokio task として管理し、independent agent contexts（各 AgentRun が独立 context）と background agent（GUI 非依存で継続、delegate_background / send_message / wait / cancel）を一級機能として提供する。role → model routing は v01-routing-profiles に委譲する。

## Why This Slice Exists Now

mvp-roadmap の v0.1 成功基準は「Orchestrator が依頼を受け、Explorer / Worker / Reviewer を background 起動し、event stream が観測できる」こと。その土台となる role 実行 runtime が無いと、GUI の agent / tasks pane（v01-gui-panes）も v0.2 の Librarian / Oracle 追加も成立しない。ADR 0002 の「prompt discipline ではなく権限で分離する」を満たすには runtime レベルでの tool 権限強制が必須で、これが本スライスの次の一歩である。

## Current Observed State

本リポジトリ（turtton/evorch）は greenfield。コードは v01-scaffold による空 crate のみで、runtime / agents の実装は存在しない。設計（intents/）は role の capability boundary（ADR 0002）、AgentRun と背景タスク（feature/agent-runtime-kernel）、v0.1 の4 role（mvp-roadmap）を定めているが、実行コードは未着手。

## Accepted Baseline You May Assume

- v01-scaffold により `crates/runtime/` と `crates/agents/` が空 crate として Scaffold 済み
- v01-event-stream が EventBus と AgentEvent（Started / MessageDelta / ToolStarted / Completed / Failed / BackgroundTaskStarted / BackgroundTaskCompleted 等）の送受信 API を提供する
- v01-session-storage が SQLite による session / AgentRun / task の永続化 API を提供する
- v01-provider-client が ProviderProfile 抽象とプロバイダ呼び出し API を提供する
- v01-tool-layer が tool trait と read / edit / grep / shell / git diff 等の実装を提供する
- v01-sandbox-approval が批准 permission 判定を提供し、tool 実行時の承認要求を決定する

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/runtime/`, `crates/agents/`

Target part: `AgentRun・role capability boundary・background 実行`

## In Scope

- `crates/agents/`: Role 定義（Orchestrator / Explorer / Worker / Reviewer）と role ごとの許可 tool セット・network 扱いの capability 定義。Librarian / Oracle を既存機構の拡張で追加できる構造
- `crates/runtime/`: AgentRun の実行管理（Tokio task）、event-sourced 状態遷移、independent agent contexts、background agent（delegate_background / send_message / wait / cancel）、role → execution policy の適用
- AgentRun の状態遷移を v01-event-stream の EventBus へ emit
- background agent の開始・完了・キャンセルの event 観測
- 複数 AgentRun の同時並行動作（各 run が独立 context）

## Out Of Scope

- role → model routing とフォールバック（v01-routing-profiles に委譲）
- Planner / Multimodal role、および Librarian / Oracle（v0.2。拡張構造のみ整える）
- provider 呼び出し詳細・capability 評価（v01-provider-client に委譲）
- プロンプト構築・cache / compaction / memory（context-engine、v0.2）
- GUI との結合（v01-gui-panes に委譲。本 packet は event emit まで）
- session / task 永続化・tools / sandbox / approval の実装（各依存 packet に委譲）

## Standalone Child Issue Contract

`crates/runtime/` と `crates/agents/` に、ADI 0002 の capability boundary（Orchestrator に mutation tool を持たせない、Explorer に write / edit / delegate を持たせない等）を強制する4 role の実行 runtime を実装する。AgentRun は event-sourced な状態遷移（pending / running / waiting / done / error）を v01-event-stream の EventBus へ emit し、Tokio task として起動・停止できる。各 AgentRun は独立 context を持ち、複数 agent が同時並行動作する。background agent は GUI 非依存で継続し、delegate_background / send_message / wait / cancel が event として観測できる。role → model の対応とフォールバックは本 packet では実装せず v01-routing-profiles に委譲する。v0.2 の Librarian / Oracle が role 定義追加だけで追加できる拡張構造を備える。

## Acceptance Criteria

- Orchestrator / Explorer / Worker / Reviewer の4 role が ADR 0002 の capability boundary で分離され、runtime レベルで許可 tool セット・network 扱いが role ごとに強制される
- 各 role の capability boundary がユニットテストで検証できる（tool 権限の許可/拒否と network 既定 deny の検査）
- AgentRun が event-sourced に状態遷移（pending / running / waiting / done / error）を event stream へ emit する
- background 起動した AgentRun が GUI 無しに継続し、背景タスクの開始・完了・キャンセルが event として観測できる
- 各 AgentRun が独立した agent context を持ち、複数 agent が Tokio task として同時に並行動作する
- Orchestrator が delegate / delegate_background / send_message / wait / cancel を持つ背景起動型の振る舞いを見せる
- Librarian / Oracle（v0.2 追加 role）が既存の capability boundary 機構への role 定義追加で拡張できることが実装構造で示される
- role → model の対応とフォールバックは v01-routing-profiles に委譲し、本 packet ではルーティングを自前実装しない

## Verification

- role 別 capability boundary のユニットテスト（Orchestrator に mutation tool を渡さない等）
- AgentRun 状態遷移テスト（pending → running → done / error、waiting 遷移、event emit）を mock provider + mock tool で検証
- background テスト（GUI 無しで成立・キャンセルが event 観測できる、send_message / wait / cancel の動作）
- 並行テスト（複数 AgentRun の context 独立性）
- `cargo test --workspace`、`cargo clippy --workspace -- -D warnings`、`git diff --check` を green にすること

## Related Links

- [features/agent-runtime-kernel/overview.md](../../../intents/evorch/features/agent-runtime-kernel/overview.md)
- [features/orchestration/overview.md](../../../intents/evorch/features/orchestration/overview.md)
- [ADR 0002: Role は capability boundary](../../../intents/evorch/decisions/0002-role-capability-boundaries.md)
- [mvp-roadmap v0.1](../../../intents/evorch/technology/mvp-roadmap.md)

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `features/agent-runtime-kernel`（primary）＋ `features/orchestration`。新規 intent node 不要
- ADR candidate: none（ADR 0002 で確定済み）
- Diagram candidate: none
- Docs update: none（closeout で確定した場合のみ mvp-roadmap の open questions 更新を検討）
- Closeout writeback expected: no

## Guide Reachability (G645)

本スライスは runtime カーネル内部の実装であり、guide の role が対向する新規の user-facing surface を追加しない（`no_role_facing_surface: true`）。ユーザー対向面は v01-gui-panes が追加する。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.