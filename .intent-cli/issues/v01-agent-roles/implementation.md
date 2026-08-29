# v01-agent-roles Implementation Packet

## Goal

v0.1 の4 role（Orchestrator / Explorer / Worker / Reviewer）を ADR 0002 の capability boundary として分離した状態で実行する **runtime** を実装する。`AgentRun`（architecture の主要データ構造、feature/agent-runtime-kernel §28 の構想）を event-sourced な状態遷移（pending / running / waiting / done / error）を伴う Tokio task として管理し、independent agent contexts（各 AgentRun が独立 context を持つ）と background agent（GUI 非依存で継続、delegate_background / send_message / wait / cancel）を一級機能として提供する。role 実行に必要な tool 権限・network 扱いの強制は `crates/agents/` の role 定義と `crates/runtime/` の policy 適用レイヤで行い、role → model の対応とフォールバックは v01-routing-profiles に委譲する（本 packet ではルーティングを実装しない）。

## Why

mvp-roadmap の v0.1 成功基準は「Orchestrator が依頼を受け、Explorer / Worker / Reviewer を background 起動し、event stream が観測できる」こと。この成功基準の土台は role を capability boundary として実行できる runtime であり、本 packet が最初に立つスライス。これが無いと v01-gui-panes（agent pane / tasks pane の描画元）も v0.2 の Librarian / Oracle 追加も成立しない。また ADR 0002 は「prompt discipline ではなく権限で分離する」ことを定めており、この分離は runtime レベルでなければ検証できない。

## Scope

- `crates/agents/`: Role 定義（Orchestrator / Explorer / Worker / Reviewer の4種）と、role ごとの許可 tool セット・network 扱いの capability 定義。Librarian / Oracle を追加できる拡張構造（role 定義追加で足りる）にする
- `crates/runtime/`: `AgentRun` の実行管理（Tokio task として起動・停止）、event-sourced な状態遷移（pending / running / waiting / done / error）、independent agent contexts、background agent（delegate_background / send_message / wait / cancel）、role → execution policy の適用
- AgentRun の状態遷移イベントを v01-event-stream の EventBus へ emit する
- background agent の開始・完了・キャンセルを event として観測可能にする
- 複数 AgentRun の同時並行動作（各 run が独立 context）

## Out of scope

- role → model routing とフォールバック（v01-routing-profiles に委譲）
- Planner / Multimodal role および Librarian / Oracle（v0.2 以降の追加 role。ただし拡張構造のみ整える）
- provider 呼び出し詳細・ProviderCapabilities 評価（v01-provider-client に委譲）
- プロンプト構築・cache / compaction・memory（context-engine、v0.2）
- GUI との結合（v01-gui-panes に委譲。本 packet は event stream への emit まで）
- session / task 永続化の詳細（v01-session-storage に委譲。利用のみ）
- tools / sandbox / approval の実装（v01-tool-layer / v01-sandbox-approval に委譲。呼び出しと permission 判定の利用のみ）

## Verification

- role 別 capability boundary のユニットテスト: Orchestrator に mutation tool を持たせない／Explorer に write / edit / delegate を許可しない、等の role → 許可 tool セットの対応を `#[cfg(test)]` で検証
- AgentRun 状態遷移テスト: pending → running → done / error と waiting への遷移、event stream への emit を mock provider + mock tool で検証
- background テスト: AgentRun を background 起動し、GUI が無い状態で成立・キャンセルが event として観測できること、send_message / wait / cancel の動作を検証
- 並行テスト: 複数 AgentRun を同時起動し、context が独立していること（一方の context の変更が他方に漏れない）を検証
- `cargo test --workspace` と `cargo clippy --workspace -- -D warnings`、`git diff --check` を実行し green にすること

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `features/agent-runtime-kernel`（primary）＋ `features/orchestration`（capability 分離）に配置。新規 intent node は不要。intent-tree は 00-map.md 単一構成のため feature overview を intent node として参照した
- ADR candidate: なし（decline）。role の capability boundary は ADR 0002 で確定済み。本 packet はその実行面の実装のみで新規 ADR は発生しない
- Diagram candidate: なし（decline）。AgentRun 状態遷移は冒頭の一覧で十分であり、専用ダイアグラムの更新を要求しない
- Docs update: なし（decline）。必須の書き戻しは無い。closeout で独立 context・background の実装方式が確定した場合のみ mvp-roadmap の open questions 更新を検討
- Closeout learning: 4 role の capability 設定値の実測結果（特に network の role-dependent 扱い、Explorer の network optional の扱い）と AgentRun 状態遷移の確定パターン。`write_back_required: false`

- Guide reachability (G645): role 実行自体は runtime カーネル内部の実装であり、guide の role が対向する新規の user-facing surface（CLI / GUI / 公開契約）を追加しない。`packet.yaml` に `no_role_facing_surface: true` を明示した。ユーザー対向面（agent / tasks pane）は v01-gui-panes が追加する。

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.

## Assumptions

- ADR 0014 の v0.1 設定領域に「permission preset」があるため、role の capability 一部を config で調整可能と想定するが、本 packet の強制対象は **builtin の role 定義**（Orchestrator に mutation tool が無い等の不可侵部分）とし、config での緩和は v01-routing-profiles の config 層に依存する
- 依存パケット（v01-event-stream / v01-session-storage / v01-provider-client / v01-tool-layer / v01-sandbox-approval）はまだ未執筆のため、それらの提供 API は本 packet の `technical_baseline` に示した想定に基づく。実装時に実際の API と乖離した場合はこの packet の記載（baseline）が勝る前提で追従する
- independent agent contexts の「context」は本 packet では AgentRun 固有のステート（message 履歴・tool 実行状態等）の分離を指し、context-engine（cache / compaction / memory）は v0.2 対象とする