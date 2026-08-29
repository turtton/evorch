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
- **Floem を第一候補**（Pure Rust / Lapce 実戦 / GPU rendering / reactive）、**egui + egui_dock を第二候補**（docking / floating / multi-window / 開発速度）。初期段階で Floem の docking prototype を作り mouse UX / dock-undock / multi-window / large transcripts を評価し、難しい場合は UI Model を保ったまま egui へ切り替えられるようにする

## 受け入れ基準

- Floem で docking prototype が動作し、panel の dock / undock / floating ができること
- Workspace Model が framework 非依存データとして保持され、GUI なしに layout を検証できること
- semantic UI API 経由で agent が panel を操作できること

## Open questions

- Floem prototype の評価結果次第での egui 切り替え基準
- 大量 transcript の描画性能要件の具体値
