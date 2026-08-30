# v01-gui-runtime-wiring Implementation Packet

## Goal

製品 `evorch-gui` を `EmptyAgentSource` から実 `AgentRuntime` へ切り替え、runtime と GUI の `EventPump` が同じ `Arc<EventBus>` を共有するよう起動配線を完成させる。同時に `AgentSummary` へ agent の表示名と実モデル識別子を追加し、tasks pane の重複 role-name / 固定 model label を実 runtime identity へ置き換える。

## Why

v0.1 inspect の slice #9 は pane 単体の実装ではなく roadmap の製品成功基準で MAJOR_DRIFT と判定された。`evorch-gui` は EventBus を作るものの `EmptyAgentSource` を渡すため（`crates/gui/src/bin/evorch-gui.rs:30-37,114-122`）、`mvp-roadmap.md:36` が要求する background agent の live 観測に到達しない。一方で runtime adapter（`crates/gui/src/model/tasks.rs:57-65`）と、共有 bus から実 runtime を組む例（`crates/runtime/examples/orchestrator_demo.rs:196-208`）は既に存在するため、v0.1.1 では新しい GUI 機能ではなく製品 entrypoint の接続 gap を閉じる。

## Scope

- `crates/gui/src/bin/evorch-gui.rs:30-37,114-139` の `EmptyAgentSource` 起動を廃止し、共有 `Arc<EventBus>`、`ToolExecutor`、外部 provider 不要の決定的 `AgentModel` 境界から `AgentRuntime` を構築する。runtime task を spawn する Tokio runtime の lifetime は GUI 終了まで維持する。
- `crates/runtime/examples/orchestrator_demo.rs:196-208,249-255` の共有 EventBus / executor / model / delegate 構成を参照する。ただし demo の script や orchestrator ロジックを製品へ無批判に複製せず、製品起動に必要な最小 lifecycle とする。
- `crates/runtime/src/run.rs:40-49` の `AgentSummary` に `name` と `model` を追加し、`crates/runtime/src/runtime.rs:37-44,68-116,167-179` の RunEntry 登録・一覧化で実行時 identity を保持・返却する。runtime が route 解決を所有しない既存境界（`crates/runtime/src/model.rs:12-29`）を尊重し、選択済み model identity を runtime へ渡す明示的な境界を設計する。
- `crates/gui/src/model/tasks.rs:76-109` の `model_label` と `role_name` 複製を削除し、`AgentSummary.name` / `role_name` / `model` を `TaskRow` へ直接写す。`WorkbenchState::new` の固定 label 引数（`crates/gui/src/app.rs:43-68`）も不要にする。
- `AgentSummary` の構築箇所、meta-op の `list_agents` serialization（`crates/runtime/src/meta.rs:187-192`）、GUI test fixtures を新契約へ更新する。
- unit / integration test で identity mapping と共有 EventBus の event flow を検証し、native GUI で live AgentRun が見える手動確認手順を文書化する。

## Out of scope

- 新規 pane、既存 pane の機能追加、layout / keybind 変更、GUI redesign。
- orchestrator の委譲戦略、role capability、provider routing / fallback の変更。
- semantic UI introspection、Diagnostics / Cost / Cache pane、v0.2 以降の機能。
- `crates/runtime/examples/orchestrator_demo.rs` のシナリオ変更。

## Verification

- `cargo test -p runtime`：`AgentSummary` の name / role_name / model / phase と meta-op serialization を検証する。
- `cargo test -p gui`：tasks mapping、state event 後の identity 保持、可能なら共有 EventBus → EventPump → WorkbenchState の headless/offscreen flow を検証する。
- `cargo run -p runtime --example orchestrator_demo`：既存 demo の完了イベントと summary が従来どおり得られることを確認する。
- 文書化した製品手順で `cargo run -p gui --bin evorch-gui -- --demo`（または実装後の同等 command）を実行し、Orchestrator / background agent の name・role・status・model と状態遷移が tasks pane に表示されることを目視確認する。外部 AI provider / credential は使わない。
- `cargo check --workspace` と `git diff --check`。

## Knowledge Maintenance (G461, optional)

- Intent placement: `features/gui-workbench/overview.md` を primary、`features/agent-runtime-kernel/overview.md`・`technology/mvp-roadmap.md`・ADR 0005 を supporting とする。新規 node は不要。
- ADR candidate: decline。headless kernel / GUI 分離と共有 Event Stream は ADR 0005 で決定済み。
- Diagram candidate: decline。既存の Agent Kernel → UI Event Bus → Workspace Model → GUI Renderer を変更しない。
- Docs update: required。`features/gui-workbench/overview.md` と `technology/mvp-roadmap.md` に製品 wiring の確定・手動確認方法を記録する。
- Closeout learning: `write_back_required: true`。製品起動 lifecycle、AgentSummary identity 境界、automated / manual verification の分担を書き戻す。
- Guide reachability (G645): route form。`guide workflow task implementation-loop` / role `implementation` から、実 runtime session と live tasks pane を持つ既存 evorch GUI app へ到達させる。GUI-adjacent ではなく operator-facing surface の実接続を完成させるため、`no_role_facing_surface: true` にはしない。

`improve` (G456 / G460) は後続の安全網であり、本 packet の docs writeback と route 宣言を closeout で確認する。
