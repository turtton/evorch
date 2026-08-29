## Goal

ADR 0007 / 0009 どおりの egui + egui_dock で v0.1 の基本3 pane（agent / terminal / tasks）を持つ GUI を実装する。GUI framework を中心にしない層構造（Agent Kernel → UI Event Bus → Workspace Model → GUI Renderer）を守り、`crates/workspace-ui/` に Split / Tabs / Panel / Floating / Window を framework 非依存データとして保持する。offscreen レンダリング（ADR 0009）でフレームを capture できる土台を作り、panel layout・keybind を config 公開する（ADR 0014）。

## Why This Slice Exists Now

mvp-roadmap v0.1 の成功基準は「GUI で複数 pane が表示される」こと。ADR 0009 は人間向け公式クライアントを GUI のみと定めており、製品の顔になる surface を最初の GUI スライスで立てる必要がある。また Workspace Model の framework 非依存分離（ADR 0007）は Floem / GPUI 切り替えを残す前提条件で、最初から守るべき。v01-agent-roles / v01-event-stream / v01-session-storage が整った段階で、その上に載る最初の renderer として実装する。

## Current Observed State

greenfield。`crates/gui/`・`crates/workspace-ui/` は v01-scaffold による空 crate のみ。GUI アプリ・Workspace Model・pane の実装は存在しない。ADR 0007（egui + egui_dock 第一候補）と ADR 0009（Linux 先行・offscreen・TUI 見送り）が設計を確定済みだがコード未着手。

## Accepted Baseline You May Assume

- v01-scaffold により `crates/gui/` と `crates/workspace-ui/` が空 crate として Scaffold 済み
- v01-agent-roles が AgentRun・role capability boundary・background agent 実行を提供し、AgentRun 一覧と状態遷移が event として観測できる
- v01-event-stream が EventBus と AgentEvent の subscribe API（transcript の message / reasoning / tool 実行）を提供する
- v01-session-storage が永続化された session / AgentRun の参照 API を提供する

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/gui/`, `crates/workspace-ui/`

Target part: `GUI アプリ・workspace model・基本 pane`

## In Scope

- `crates/workspace-ui/`: Workspace Model（Split / Tabs / Panel / Floating / Window の framework 非依存データ）、layout の検証・保存（egui_dock の DockEvent / persistence と接続）、panel 定義（agent / terminal / tasks）
- `crates/gui/`: egui + egui_dock アプリ、UI Event Bus からの購読、Workspace Model → GUI Renderer の変換、agent pane（event stream の transcript）、terminal pane（portable-pty）、tasks pane（AgentRun 一覧）、offscreen レンダリング抽象
- panel layout・keybind の config 公開（ADR 0014 の v0.1 設定領域）
- offscreen レンダリングでフレームを capture（ヘッドレス起動の土台）

## Out Of Scope

- semantic UI introspection（ui.inspect / ui.screenshot 等。v0.5）
- Cost / Cache Inspector・Diff / Diagnostics pane 等の高度 pane（v0.2 以降）
- 大容量 transcript 向け virtualization widget（v0.2。本 packet は基本描画のみ）
- Floem 評価用 prototype（任意の並行調査。必須ではない）
- TUI（ADR 0009 で製品としては不採用）
- ネットワーク・provider 呼び出し（kernel 側に委譲。GUI は購読・表示のみ）

## Standalone Child Issue Contract

`crates/gui/` と `crates/workspace-ui/` に、egui + egui_dock（anhosh/egui_dock 0.21.x、ADR 0007）による v0.1 基本3 pane（agent / terminal / tasks）の GUI を実装する。Workspace Model は framework 非依存データ（Split / Tabs / Panel / Floating / Window）として `crates/workspace-ui/` に保持し、Agent Kernel → UI Event Bus → Workspace Model → GUI Renderer の層構造を守る。agent pane は v01-event-stream の subscribe API から transcript（message / reasoning / tool 実行）を描画し、terminal pane は portable-pty で PTY を扱い、tasks pane は v01-agent-roles の AgentRun 一覧（name / role / status / model）を表示する。offscreen レンダリング（ADR 0009）でフレームを capture できる。panel layout・keybind は ADR 0014 の config から公開する。初期3 pane は egui_dock の binary split 制約のため nested split で構成し、semantic UI introspection と高度 pane は対象外。

## Acceptance Criteria

- egui + egui_dock で agent / terminal / tasks の基本3 pane が表示でき、dock / undock / resize / tab 移動が動作する
- Agent Kernel → UI Event Bus → Workspace Model → GUI Renderer の層構造を守り、Workspace Model が `crates/workspace-ui/` に framework 非依存データとして保持され、GUI 無しに layout を検証・保存できる
- agent pane が event stream の transcript（message / reasoning / tool 実行）を描画する
- terminal pane が portable-pty で PTY を生成・操作できる
- tasks pane が AgentRun 一覧を表示し、背景タスクの状態と連動する
- offscreen レンダリングでフレームを capture でき、ヘッドレス起動できる
- panel layout・keybind が config 公開される（ADR 0014 の v0.1 設定領域）
- semantic UI introspection（v0.5）と Cost / Cache Inspector・Diagnostics 等の高度 pane（v0.2 以降）が実装されないことが明示される

## Verification

- egui_dock テスト: 3 pane の dock / undock / resize / tab 移動をテストヘッドで確認
- Workspace Model テスト: layout の作成・検証・保存が GUI 起動なしで通る
- agent pane: mock event stream から transcript を描画
- terminal pane: portable-pty で入出力が往復する
- tasks pane: mock の AgentRun 一覧を表示し状態変化で更新
- offscreen テスト: フレームを image として capture できる
- `cargo test --workspace`、`cargo clippy --workspace -- -D warnings`、`git diff --check` を green にすること
- テストは headless（offscreen / test コンテキスト）で実行し、イベントループは UI Event Bus への注入で行う

## Related Links

- [features/gui-workbench/overview.md](../../../intents/evorch/features/gui-workbench/overview.md)
- [ADR 0007: GUI 第一候補を egui + egui_dock に](../../../intents/evorch/decisions/0007-gui-framework-egui-first.md)
- [ADR 0009: v0.1 プラットフォーム Linux 先行・クライアント形態](../../../intents/evorch/decisions/0009-platform-linux-first-gui-only.md)
- [ADR 0014: 設定アーキテクチャ（v0.1 の panel layout・keybind 設定領域）](../../../intents/evorch/decisions/0014-config-architecture.md)
- [mvp-roadmap v0.1（basic GUI panes）](../../../intents/evorch/technology/mvp-roadmap.md)

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `features/gui-workbench`（primary）。新規 intent node 不要
- ADR candidate: none（ADR 0007 / 0009 で確定済み）
- Diagram candidate: none
- Docs update: none（nested split 構成が確定した場合のみ overview の open questions 更新を検討）
- Closeout writeback expected: no

## Guide Reachability (G645)

本スライスはユーザー対向の GUI アプリ（evorch GUI workbench）という新規 role-facing surface を追加する。route を以下に宣言する:

- guide surface: `guide workflow task implementation-loop`
- role: `implementation`
- target surface: the evorch GUI app（egui workbench、ユーザー対向面）

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.