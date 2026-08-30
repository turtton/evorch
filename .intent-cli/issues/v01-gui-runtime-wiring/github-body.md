## Goal

製品 `evorch-gui` を実 `AgentRuntime` と共有 `EventBus` へ接続し、tasks pane が実行中 agent の name / role / status / model を live 表示するようにする。

## Why This Slice Exists Now

v0.1 inspect の slice #9 では基本 pane 自体は実装済みだが、roadmap-level product success は MAJOR_DRIFT だった。`intents/evorch/technology/mvp-roadmap.md:36` は Orchestrator が background agent を起動し event stream と GUI で観測できることを成功基準とするが、製品 binary は `EmptyAgentSource` と独自 bus を使い（`crates/gui/src/bin/evorch-gui.rs:30-37,114-122`）、実 runtime session を表示しない。この packet は pane を増やさず、既存 runtime adapter と demo wiring を製品 entrypoint へ接続する。

## Current Observed State

- `crates/gui/src/bin/evorch-gui.rs:30-37` の `EmptyAgentSource::list` は常に空を返す。
- 同 binary は `crates/gui/src/bin/evorch-gui.rs:117-122` で EventBus / EventPump を作るが、その bus を AgentRuntime へ渡さず、tasks source は空のままである。
- `crates/gui/src/model/tasks.rs:57-65` には `AgentRuntime: AgentRunSource` adapter が既にある。
- `AgentSummary` は `crates/runtime/src/run.rs:40-49` で run_id / role_name / phase のみを持ち、`crates/runtime/src/runtime.rs:167-179` も name / model を返さない。
- `crates/gui/src/model/tasks.rs:76-109` は name を role_name から複製し、model を constructor の固定 label から全行へ複製する。
- 実際の共有 wiring は `crates/runtime/examples/orchestrator_demo.rs:196-208,249-255` にあり、同じ EventBus で runtime と subscriber を接続している。

## Accepted Baseline You May Assume

- Rust edition 2024 / rust-version 1.97、Tokio 1、serde 1、tracing 0.1（`Cargo.toml:5-8,28,32,35`）。
- eframe / egui 0.36、egui_dock 0.21（`Cargo.toml:13-17`）。
- AgentRuntime は `Arc<EventBus>` / `ToolExecutor` / `AgentModel` から構築できる（`crates/runtime/src/runtime.rs:51-66`）。
- `delegate_background` は run 登録と lifecycle event を同じ bus へ発行する（`crates/runtime/src/runtime.rs:68-116`）。
- GUI の EventPump / WorkbenchState と AgentRuntime adapter は実装済み。

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/gui/`, `crates/runtime/`

Target part: `evorch-gui` 起動配線、`AgentSummary` identity、tasks pane runtime 表示。

## In Scope

- 製品 binary で実 AgentRuntime と EventPump が同一 `Arc<EventBus>` を共有する起動・shutdown lifecycle。
- 外部 provider / credential 不要で live agent 表示を再現できる決定的 runtime session。
- `AgentSummary` への name / model 追加と runtime からの実データ設定。
- tasks model / WorkbenchState から固定 model label と role-name 複製を除去。
- runtime / GUI focused tests、既存 orchestrator demo の回帰確認、native GUI 手動確認手順。

## Out Of Scope

- 新しい pane / feature、GUI redesign、layout / keybind の再設計。
- orchestrator logic、role capability、provider routing の変更。
- semantic UI、Diagnostics、Cost / Cache Inspector。
- orchestrator demo の挙動変更。

## Standalone Child Issue Contract

`turtton/evorch` の製品 `evorch-gui` entrypoint を、`EmptyAgentSource` ではなく実 `AgentRuntime` と共有 EventBus で動かしてください。`AgentSummary` は name / role_name / model / phase を実行時データとして返し、GUI tasks model は固定 label や role の複製ではなくその DTO を表示します。外部 AI provider 無しで live agent と状態遷移を確認できる起動方法を用意・文書化し、runtime / GUI の自動テストと既存 `crates/runtime/examples/orchestrator_demo.rs` の回帰確認を行ってください。新規 pane、orchestrator / routing 変更、GUI redesign は含めません。

## Acceptance Criteria

- [ ] `evorch-gui` の AgentRuntime と EventPump が同一の `Arc<EventBus>` を共有し、`EmptyAgentSource` が製品起動経路から無くなる。
- [ ] 外部 AI provider 無しの runtime session で Orchestrator と background AgentRun が tasks pane に live 表示され、手動確認手順と期待状態が記載される。
- [ ] `AgentSummary` が name / model を持ち、`AgentRuntime::list_agents` が各 run の実 identity を設定する。
- [ ] tasks model が summary の name / role_name / model を直接表示し、固定 model label と role-name の name への複製が無くなる。
- [ ] runtime 一覧 / serialization と GUI mapping の tests が異なる name / role / model および状態更新後の identity 保持を検証する。
- [ ] 共有 EventBus の lifecycle event が EventPump 経由で GUI model へ反映される範囲を自動テストし、native 描画部分は記載した手順で目視確認する。
- [ ] `crates/runtime/examples/orchestrator_demo.rs` のシナリオに挙動変更がなく、既存 runtime / GUI tests と demo verification が通る。
- [ ] 新規 pane、orchestrator logic、routing、GUI redesign を含めない。

## Verification

- `cargo test -p runtime`
- `cargo test -p gui`
- `cargo run -p runtime --example orchestrator_demo`
- 文書化した `evorch-gui` demo command を実行し、tasks pane の agent identity と状態遷移を目視確認（外部 provider 不使用）。
- `cargo check --workspace`
- `git diff --check`

## Related Links

- `intents/evorch/features/gui-workbench/overview.md:23-38`
- `intents/evorch/features/agent-runtime-kernel/overview.md:24-46`
- `intents/evorch/decisions/0005-headless-kernel-and-gui-separation.md:11-33`
- `intents/evorch/technology/mvp-roadmap.md:7-36`
- v0.1 slice #9 / issue #9 (`v01-gui-panes`)

## Knowledge Maintenance

- Intent placement: `features/gui-workbench/overview.md`（primary）、agent-runtime-kernel / roadmap / ADR 0005（supporting）。新規 node なし。
- ADR candidate: none。ADR 0005 の既存境界を実装へ接続する。
- Diagram candidate: none。既存層構造を維持する。
- Docs update: `features/gui-workbench/overview.md` と `technology/mvp-roadmap.md` に製品 wiring と手動確認を記録する。
- Closeout writeback expected: yes。

## Guide Reachability (G645)

Route form: `guide workflow task implementation-loop` → role `implementation` → `the evorch GUI app（実 AgentRuntime session と live tasks pane）`。既存 GUI surface の実 runtime 到達性を完成させる変更なので no-surface form は使わない。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
