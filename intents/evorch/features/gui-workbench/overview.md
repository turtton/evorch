# Feature: GUI Workbench（ネイティブ GUI ワークベンチ）

[features 一覧](../) / [agent-runtime-kernel](../agent-runtime-kernel/overview.md) / [technology/architecture](../../technology/architecture.md)

## 概要

TUI に限定しない。目標は **IDE / Workbench 的な Native GUI** で、Qt の Dock Widget のように各機能を自由に配置できること。

```text
┌──────────────────────────────────────────────────┐
│ Tasks │ Main Agent               │ Explorer #1  │
│       │                          ├───────────────┤
│       │                          │ Librarian #2 │
│       │                          ├───────────────┤
│       │                          │ Worker #3    │
├───────┴──────────────────────────┴───────────────┤
│ Terminal │ Diff │ Diagnostics │ Cache │ Provider│
└──────────────────────────────────────────────────┘
```

Panel は left / right / bottom / tabs / floating / separate OS window に自由に配置できる。

## 要件

- **Subagent の可視化**: background agent を「裏で動いている何か」にしない。可能なら全 agent を表示し、難しくてもデフォルトで3つ程度を常時表示。各 Agent Panel で status / role / model / provider / reasoning / tool execution / transcript / cache / usage を確認できる
- **GUI framework と Workspace Model の分離**: GUI framework を application architecture の中心にしない。Agent Kernel → UI Event Bus → Workspace Model → GUI Renderer の層構造。Workspace Model / Layout（Split / Tabs / Panel / Floating / Window）は framework-independent data として保持し、Floem から egui への切り替えが可能にする
- **Semantic UI API**: Agent から GUI を pixel surface として扱わせず、semantic object graph として expose する（ui.inspect / ui.find / ui.open_panel / ui.close_panel / ui.move_panel / ui.focus / ui.set_layout / ui.save_workspace / ui.screenshot）。GUI 自体も agent から理解・改善可能にする
- **UI 自己改善（3段階）**: Level 1 runtime configuration（pane placement / filters / keybind 等の即時変更）、Level 2 UI composition（既存 primitive の組み合わせによる新 view: Cache Dashboard 等）、Level 3 framework implementation（Rust source 変更が必要なものは worktree → build → test instance → semantic inspection → screenshot / interaction replay で自己検証）
- **GUI framework 選定（ADR 0007、2026-08 再評価で確定）**: 第一候補は **egui + egui_dock**（`anhosh/egui_dock` 0.21.x。tab 移動/resize/undock/floating window、DockEvent による layout persistence を標準提供、2026 年も活発リリース）。Floem は汎用 dock API を提供せず安定版が v0.2.0（2024-11）のままであるため「docking 評価用 prototype」に限定。**GPUI + gpui-component**（Zed 実戦系、dock/nested split/floating/syntax highlighting 対応）は長期 watch
- **大容量 transcript の扱い**: 行単位 chunking + 差分更新 + 明示的 virtualization の自前 widget とし、framework 非依存設計にする（egui immediate mode の制約。Floem/GPUI への切り替え時も流用可能に）

## v0.1.1 実 runtime wiring の実装確定（2026-08-30、PR #30 / issue #29）

製品 GUI entrypoint（`crates/gui/src/bin/evorch-gui.rs`）が `EmptyAgentSource` を廃止し、実 `AgentRuntime` と同一 `Arc<EventBus>` を EventPump と共有する構成で landed。

- **製品起動 lifecycle**: `evorch-gui` は起動時に `AgentRuntime::production(bus, policy, workspace_root, model)` を組み、失敗時（bwrap 未検出）は明確なエラーで exit 1（fail-closed）。`--demo` は外部 AI provider / credential 不要の決定的 scripted session（orchestrator が background worker を delegate）を起動し、各 row が Pending → Running → Done へ遷移する。手動確認手順は `--help` に同梱
- **AgentSummary identity 境界**: `AgentSummary { name（表示名）, role_name（role）, model（実行 model）, phase }`。`RunConfig.name` 未指定時は name は role 名へフォールバック。model は `AgentModel::selected_model(role)`（routing profile 層が報告、runtime は解決しない）をそのまま記録。`list_agents` serialization と GUI tasks pane はこの identity を直接写像する（TaskRow への固定 label・role→name 複製は廃止）
- **自動 / 手動検証の分担**: headless wiring test（`gui/tests/runtime_wiring.rs`: 実 runtime + EventPump + WorkbenchState で 2 rows 収束）は CI で常時実行。文字内容レイアウト（name/role/model の実表示）は headless screenshot 基盤が必要で別 unit。`--demo` の手順による目視は手動
- **network capability 伝播**: production runtime は `runtime::network::build_sandbox(policy)` 経由で role の network mode を bwrap へ伝播する（PR #20 seam。`AgentRuntime::production` 内では `ToolExecutor::with_standard_tools` に構築済み sandbox を注入。`with_production_sandbox` は BwrapConfig 直接受取りの sibling entry として残存）

## 受け入れ基準

- egui + egui_dock で基本 pane（agent / terminal / tasks 等）の dock / undock / floating ができること（landed）
- 製品 GUI が実 AgentRuntime から tasks pane を live 表示できること（landed、PR #30。`--demo` で外部 provider 不要の確認経路あり）
- Workspace Model が framework 非依存データとして保持され、GUI なしに layout を検証できること
- semantic UI API 経由で agent が panel を操作できること
- 仮想化 transcript widget が1万行規模の transcriptで操作が追従すること
- **offscreen レンダリングによるヘッドレス起動**が可能であること（自己改善の test instance / capture_ui 用。ADR 0009）

## Related decisions

- [ADR 0005: Headless Agent Kernel と GUI の分離](../../decisions/0005-headless-kernel-and-gui-separation.md)
- [ADR 0006: Harness 自身の診断と自己改善](../../decisions/0006-self-improvement-and-diagnostics.md)
- [ADR 0007: GUI 第一候補を egui + egui_dock に](../../decisions/0007-gui-framework-egui-first.md)

## Open questions

- Floem 評価用 prototype の実施タイミング（v0.2 並行で十分か）
- 大量 transcript の描画性能要件の具体値（目標フレームレート・行数の定量値）
