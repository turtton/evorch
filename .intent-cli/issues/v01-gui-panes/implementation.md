# v01-gui-panes Implementation Packet

## Goal

ADR 0007 / 0009 どおりの egui + egui_dock で、v0.1 の基本3 pane（agent / terminal / tasks）を持つ GUI を実装する。GUI framework を application architecture の中心にしない層構造、すなわち **Agent Kernel → UI Event Bus → Workspace Model → GUI Renderer** を守り、`crates/workspace-ui/` に Split / Tabs / Panel / Floating / Window を framework 非依存データとして保持する。offscreen レンダリング（ADR 0009）でフレームを capture できる土台を作り、将来の visual-qa / capture_ui に備える。panel layout・keybind は config 公開する（ADR 0014 の v0.1 設定領域）。

## Why

mvp-roadmap の v0.1 成功基準は「GUI で複数 pane が表示される」こと。また ADR 0009 は人間向け公式クライアントを GUI のみと定め、これは製品の顔になる surface である。workspace model の framework 非依存分離（ADR 0007 consequence）は、Floem / GPUI への将来切り替えを選択肢に残すための前提で、v0.1 の最初の GUI スライスで守る必要がある。v01-agent-roles / v01-event-stream / v01-session-storage が整った段階で、その上に載る最初の renderer として本 packet を立てる。

## Scope

- `crates/workspace-ui/`: Workspace Model（Split / Tabs / Panel / Floating / Window の framework 非依存データ）、layout の検証・保存（egui_dock の DockEvent / persistence と接続）、panel 定義（agent / terminal / tasks）
- `crates/gui/`: egui + egui_dock アプリ（ADR 0007）、UI Event Bus からの購読、Workspace Model → GUI Renderer の変換、agent pane（event stream の transcript: message / reasoning / tool 実行）、terminal pane（portable-pty）、tasks pane（AgentRun 一覧）、offscreen レンダリング抽象（ADR 0009 の初期設計要素）
- panel layout・keybind の config 公開（ADR 0014 の v0.1 設定領域）
- offscreen レンダリングでフレームを capture（ヘッドレス起動の土台）

## Out of scope

- semantic UI introspection（ui.inspect / ui.screenshot 等。v0.5）
- Cost / Cache Inspector・Diff / Diagnostics pane 等の高度 pane（v0.2 以降）
- 1万行規模の 大容量 transcript 向け virtualization widget（v0.2。本 packet は基本描画のみ）
- Floem 評価用 prototype（任意の並行調査。必須ではない）
- TUI（ADR 0009 で製品としては不採用）
- ネットワーク・provider 呼び出し（すべて kernel 側に委譲。GUI は購読・表示のみ）

## Verification

- egui_dock テスト: agent / terminal / tasks の3 pane が dock され、undock / resize / tab 移動が動作することをテストへッドで確認
- Workspace Model テスト: layout の作成・検証・保存が GUI を起動せずに通る（framework 非依存）
- agent pane: mock event stream から transcript を描画（subscribe API 経由）
- terminal pane: portable-pty で PTY を起動・操作し入出力が往復する
- tasks pane: mock の AgentRun 一覧を表示し状態変化で更新される
- offscreen テスト: offscreen レンダリングでフレーム（image）を capture でき、capture_ui で再利用する形式に保存できる
- `cargo test --workspace`、`cargo clippy --workspace -- -D warnings`、`git diff --check` を green にすること
- egui_dock は headless ビルドのため、テストは offscreen / test コンテキストで実施する（イベントループは UI Event Bus への注入）

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `features/gui-workbench`（primary）。新規 intent node は不要（intent-tree は 00-map.md 単一構成のため feature overview を intent node として参照）
- ADR candidate: なし（decline）。GUI 選定は ADR 0007、offscreen / Linux 先行は ADR 0009 で確定済み。本 packet は実装のみ
- Diagram candidate: なし（decline）。pane 配置の概念図は gui-workbench overview に既存。実際の nested split 構成が確定した場合のみ更新を検討
- Docs update: なし（decline）。必須の書き戻しなし。egui_dock の制約下で good に確定したレイアウト構成は closeout 学習として回収し、必要時のみ overview の open questions を更新
- Closeout learning: egui_dock の binary split 制約（3分割以上・grid 非標準）で初期3 pane を nested split で実現した具体構成と、offscreen レンダリングによるフレーム capture の確定方式。`write_back_required: false`

- Guide reachability (G645): 本スライスはユーザー対向の GUI アプリ（evorch GUI workbench）という新規 role-facing surface を追加する。`packet.yaml` に route を宣言した: guide workflow task implementation-loop → role: implementation → target_surface: the evorch GUI app（egui workbench）。

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.

## Assumptions

- ADR 0007 consequence の「egui_dock の binary split 制約（3分割以上・grid 非標準）を受け入れる。初期画面の3ペイン構成は nested split で実現する」という前提で、3 pane の初期 layout は nested split 構成にする
- offscreen レンダリングは ADR 0009 の「egui の surface を offscreen render 可能にする抽象を v0.1 の初期設計に含める」に従い、GUI アプリ本体と切り離した capture 機能として実装する
- agent pane の transcript 描画は本 packet では基本表示（仮想化なし）とする。行単位 chunking + virtualization の自前 widget は gui-workbench 要件の大容量対応（v0.2）で追加する前提の設計に保つ