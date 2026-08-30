# v02-otel-span-exporter Implementation Packet

## Goal

metrics unit（slice 1: v02-otel-metrics-exporter）が `crates/event-bus/` 内に導入した写像層（domain event → OTel 変換 module）を span に拡張し、委譲親子を OTel span 親子として OTLP export できる状態にする。ADR 0023 の二段分割における slice ②。内部 event（ドメイン語彙を保持）から task/run span → agent 実行 span（`invoke_agent`）→ child `chat` → `execute_tool` の階層を生成し、slice 1 で導入される feature gate（opt-in 既定 off）を維持したまま span OTLP exporter から送出する。

## Why

ADR 0023 で signals = metrics + span、委譲親子 = span 親子、写像層集中、slice 二段分割（① metrics、② span 本 slice）が確定した。span の親子紐付けは stateful で難度が高く、① とは別 slice に集中するのが決定 5 の趣旨。mvp-roadmap の v0.2「役割の深化と観測」において、① が metrics 側の exporter を確立し、本 slice で span 側まで揃えて観測フェーズを完了させる。なお span の可視化 pane は gui-workbench 側の別 slice の責務であり、本 slice は export 面のみを扱う。これが role-facing surface を追加しない（`no_role_facing_surface: true`）判断の理由である。

## Scope

- 写像層（`crates/event-bus/` 内、slice 1 が導入した domain event → OTel 変換 module）の span 化拡張:
  - 内部 event → span 区間化（request / agent_run span）と親子紐付け。event schema の task_id / agent_run_id 相関（PR #32 で landed した provider 観測 event 群を含む）と ADR 0022 の親子ツリー addressing を用い、委譲親子（orchestrator → 委譲先 agent 実行）を span 親子（parent span）へ写像する。
  - `gen_ai.*` 標準属性の付与（`invoke_agent` / `chat` / `execute_tool` 階層、operation name、request model、usage 系。pin 対象 semconv リリースは写像表に明記、現基準 v1.37.0）。
  - `evorch.*` span 属性で session / task / agent_run ID を保持（metrics 側には載せない高カーディナリティ規律）。構造軸は低カーディナリティ列挙（`evorch.delegation.depth` / `evorch.delegation.role`）に留める。
- span OTLP exporter: slice 1 の exporter 基盤（slice 1 で導入される opentelemetry crate 依存と feature gate、ADR 0014 由来の config 構成）を span 面に拡張し、`crates/event-bus/` から送出する。
- sampling hook と ADR 0012 のハード上限（件数/時間/byte 閾値）を span 経路にも適用。
- 検証基盤の span 拡張: golden/snapshot fixture（span 親子構造、pin 固定）、CI cardinality guard（ID 混入の静的ブロック）、debug exporter 経由の最小 OTLP E2E（span）1 本。
- slice 着手時に semconv pin（現基準 v1.37.0）の bump を検討する（ADR 0023 決定 6）。bump する場合は「release 差分精読 → 写像表差分 → 検証結果」の明示表を残す。

## Out of scope

- metrics 面の変更（slice 1 の責務。回帰させないことは review 観点）。
- span の可視化 pane 実装（gui-workbench 側の別 slice の責務。本 slice は export 面のみ）。
- raw の LLM I/O / SSE body / message 本文の収集・export（ADR 0012 の raw 非永続ポリシーに基づく恒久対象外。raw 非永続の射程に注意）。
- cost / 料金計算指標の変更。
- semconv pin（現基準 v1.37.0）からの無断 latest 追従（pin + 意図的 bump ポリシーのみ）。
- 内部 event schema の OTel 標準名への侵食（ドメイン語彙を保持）・producer 層（provider / runtime / tool）による opentelemetry crate の直接参照。
- slice 1 が導入する feature gate の既定 on 化・強制有効化。

## Verification

- unit test:
  - 委譲 fixture に対し span 親子（parent span 紐付け）が正しく生成される（golden/snapshot test、pin 固定 fixture）。
  - span 属性に raw payload / message 本文 / credential が混入しないことの fixture 検証。
  - sampling hook 既定値とハード上限（件数/時間/byte 閾値）の span 経路への適用確認。
  - feature off 状態でも既存の内部計測と slice 1 の metrics 側 export が回帰しない。
- CI cardinality guard が span 属性に対しても ID 混入を静的ブロックする（metrics whitelist の形骸化防止）。
- 写像層 → OTel SDK exporter → debug exporter の JSON 出力 assertion による最小 OTLP E2E（span）1 本。
- `cargo test -p event-bus` / `cargo clippy -p event-bus -- -D warnings` / `cargo fmt --check` が通ること。`git diff --check`。

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: primary = features/diagnostics-self-improvement/overview.md（委譲計測・観測は自己改善機能の分析基盤の入力源。ADR 0023 Context）、supporting = technology/mvp-roadmap.md（v0.2「役割の深化と観測」）と features/gui-workbench/overview.md（可視化 pane は別 slice）。新規 intent node は不要。独立した OTel intent は作らない。slice 1 と同一 placement の span 側拡張。
- ADR candidate: 条件付き decline。ADR 0023 は確定済み前提。ただし本 slice 実装中に semconv pin を bump した場合のみ、ADR 0023 へ change-log 追記（release 差分精読 → 写像表差分 → 検証結果の明示表）が必須。
- Diagram candidate: なし（task/run → agent → invoke_agent → chat → execute_tool の階層は ADR 0023 決定 2・決定 4 と本 packet のテキストで表現済み）。
- Docs update: `intents/evorch/technology/mvp-roadmap.md` の v0.2「役割の深化と観測」に OTLP exporter（span 側まで含む）の完了を反映する。
- Closeout learning: mvp-roadmap.md の反映が必須。semconv bump 実施時のみ ADR 0023 change-log 追記が条件付き必須。`write_back_required: true`。

- Guide reachability (G645): for every role-facing surface, name the guide surface, routing role,
  and target surface; if none is added, explicitly set `no_role_facing_surface: true`. A blank
  declaration is not a decision. `stalled-work` reports a declared route until the host records it.

本 slice は kernel 側の export 面のみを扱い、role-facing surface を追加しない（span の可視化 pane は gui-workbench 側の別 slice の責務）。`no_role_facing_surface: true` を明示する。

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.
