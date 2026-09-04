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

## v0.2 GUI 再構成の確定（grill grill-v02-loop-foundation、2026-09-02）

t3code（pingdotgg/t3code、commit b883fc0 調査）を基準レイアウトとして採用する。egui_dock の自由配置機構は保持し、以下を**既定レイアウト**とする。

```text
┌──────────────┬──────────────────────────────┬────────────────────┐
│ 左サイドバー   │ 中央: 会話 (thread)          │ 右: tabbed surfaces │
│ project 管理  │                              │  Agents (主眼)      │
│ thread 管理   │                              │  Diff (最小)        │
└──────────────┴──────────────────────────────┴────────────────────┘
```

- **プロジェクト概念**: プロジェクトは「基準 repo/path + アクセス許可ディレクトリ集合」を持つ。subagent worktree の cwd はプロジェクトルートと一致しないため（cwd != プロジェクト）、プロジェクトごとにアクセス可能ディレクトリを設定し、sandbox 境界・project trust（ADR 0008 v0.2 項目）と一体化する。worktree（`evorch/task/<run-id>`）はプロジェクト許可ディレクトリ傘下として自動許可
- **thread 管理**: 複数 session の作成/切替/pin/状態表示を左サイドバーで提供（v0.2 スコープ）
- **Agents 可視化（主眼、t3code を超える水準）**: 右サイドバー Agents tab は一覧（identity/phase/model/provider/現在 tool/token・usage）のライブ更新 + 選択 agent の中央 pane drill-down + dock 機構による複数 agent transcript pane 同時ライブ。既定レイアウトは orchestrator + 直近 worker + reviewer の3分割程度。t3code の Agents tab は dashboard 止まりなので、ここは独自実装
- **diff tab（最小版のみ v0.2）**: working tree / branch の unified diff 表示（人間 merge 承認の判断材料）。file tree / turn 別 diff / whitespace 制御等の完全版は v0.3 以降
- **loop UI**: goal 投入（goal + packet/issue 参照 + 制約）と merge 承認 approval は本 feature が器を提供し、orchestrator-loop の機構が利用する

## v0.1.1 実 runtime wiring の実装確定（2026-08-30、PR #30 / issue #29）

製品 GUI entrypoint（`crates/gui/src/bin/evorch-gui.rs`）が `EmptyAgentSource` を廃止し、実 `AgentRuntime` と同一 `Arc<EventBus>` を EventPump と共有する構成で landed。

- **製品起動 lifecycle**: `evorch-gui` は起動時に `AgentRuntime::production(bus, policy, workspace_root, model)` を組み、失敗時（bwrap 未検出）は明確なエラーで exit 1（fail-closed）。`--demo` は外部 AI provider / credential 不要の決定的 scripted session（orchestrator が background worker を delegate）を起動し、各 row が Pending → Running → Done へ遷移する。手動確認手順は `--help` に同梱
- **AgentSummary identity 境界**: `AgentSummary { name（表示名）, role_name（role）, model（実行 model）, phase }`。`RunConfig.name` 未指定時は name は role 名へフォールバック。model は `AgentModel::selected_model(role)`（routing profile 層が報告、runtime は解決しない）をそのまま記録。`list_agents` serialization と GUI tasks pane はこの identity を直接写像する（TaskRow への固定 label・role→name 複製は廃止）
- **自動 / 手動検証の分担**: headless wiring test（`gui/tests/runtime_wiring.rs`: 実 runtime + EventPump + WorkbenchState で 2 rows 収束）は CI で常時実行。文字内容レイアウト（name/role/model の実表示）は headless screenshot 基盤が必要で別 unit。`--demo` の手順による目視は手動
- **network capability 伝播**: production runtime は `runtime::network::build_sandbox(policy)` 経由で role の network mode を bwrap へ伝播する（PR #20 seam。`AgentRuntime::production` 内では `ToolExecutor::with_standard_tools` に構築済み sandbox を注入。`with_production_sandbox` は BwrapConfig 直接受取りの sibling entry として残存）


## v0.2 GUI workbench 3領域再構成の実装確定（issue #65、PR #66、2026-09-05）

- workspace schema v2 + `migrate.rs` による v1 自動移行。PanelKind に Sidebar / Agents / AgentTranscript（target=run_id 必須）/ Diff / Goal / Merge を追加し、load は fail-closed（不正 panel 参照・duplicate・invalid trust path を拒否）
- project/thread/trust モデルは workspace-ui 所有（config 不変）。allowed-directory は mutation/load で同一 validator を通し、runtime 所有 worktree（`<root>/.evorch/worktrees/`）は membership 自動許可、任意外部 path は明示 trust なしに許可しない
- TranscriptRegistry が run-addressed event（tool は run_id、approval は call_id index、AgentMessage は sender/recipient）を決定配送。MessageDelta は run_id を持たないため thread + 単一 Running run のみ mirror（follow-up 候補: event-bus の MessageDelta/ReasoningDelta へ run_id 付与）
- Telemetry は ProviderEvent の run_id Some のみ集約、UsageEvent は推測せず無視
- Diff は working tree / main 固定 branch のみ（turn 別・split・whitespace・file tree・base 任意選択は v0.3 scope 据置）、256KiB cap、空/Error は明示状態
- goal-submission / merge-approval は WorkbenchCommand + FixtureLoopAdapter で型付き・決定的（orchestrator-loop 未接続でも fixture で操作可能）
- `--demo` は 3 役 delegate + AgentMessage + telemetry + diff/goal/merge を外部 provider なしで再現、`--state` で sidebar 永続化、手動確認手順は `--help` に同梱（NixOS は LD_LIBRARY_PATH+WGPU_BACKEND=gl+llvmpipe が必要）
- HeadlessWorkbench 統合テスト（chained scenario + v1 migration + error paths）を含む。GUI 95 / workspace-ui 38 test green、Reviewer Gate 3 round で APPROVED（AC5 非混線・AC10 migration・AC11 scope 外項を最終修正）
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
