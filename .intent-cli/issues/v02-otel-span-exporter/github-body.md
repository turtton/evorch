## Goal

metrics unit（slice 1: v02-otel-metrics-exporter）の写像層を span に拡張し、委譲親子を OTel span 親子として OTLP export できる状態にする。ADR 0023 の二段分割における slice ②。`crates/event-bus/` の内部 event（ドメイン語彙を保持）から task/run span → agent 実行 span（`invoke_agent`）→ child `chat` → `execute_tool` の階層を生成し、slice 1 で導入される feature gate（opt-in 既定 off）を維持したまま span OTLP exporter から送出する。

## Why This Slice Exists Now

ADR 0023 で signals = metrics + span、委譲親子 = span 親子、写像層集中、exporter 二段分割（① metrics、② span）が確定した。span 親子紐付けは stateful で難度が高いため独立 slice に集中するのが決定 5 の趣旨で、① の写像層・metrics exporter 完了が本 slice の着手条件。mvp-roadmap v0.2「役割の深化と観測」の exporter 面は、① の metrics に続いて本 slice の span で揃う。なお span の可視化 pane は gui-workbench 側の別 slice の責務であり、本 slice は export 面のみを扱う（role-facing surface を追加しない判断の根拠）。

## Current Observed State

- 内部 event schema は `crates/event-bus/` に存在し、event bus の in-process broadcast transport（ADR 0017）で複数 subscriber に配信される。
- PR #32 で provider 観測 event（request 開始/TTFT/完了/失敗/fallback、task_id / agent_run_id 相関）が `crates/event-bus/` に landed 済み。
- OTel 依存・写像層・OTLP exporter はいずれも未導入。slice 1（v02-otel-metrics-exporter）が ① の写像層と metrics exporter を新規導入する前提。
- 委譲親子を外部 collector の trace として観測できる経路は存在しない。
- 委譲親子木の addressing は ADR 0022 で確立済み。

## Accepted Baseline You May Assume

- slice 1（v02-otel-metrics-exporter）完了済み: `crates/event-bus/` 内に写像層 module（domain event → OTel 変換）、feature gate された OTLP metrics exporter、golden/snapshot test + CI cardinality guard 基盤が landed（opentelemetry crate 依存と feature gate 本体は slice 1 で導入される）。
- event bus の in-process broadcast transport は ADR 0017 で確定済み。内部 event schema はドメイン語彙を保持する。
- ADR 0012 の計測アーキテクチャ（OTel Metrics API 採用、raw 非永続、ハード上限、sampling 方針）を前提とする。feature gate の config 構成・既定 off は ADR 0014、exporter 設定の reload は ADR 0019 に従う。
- 委譲親子木の addressing は ADR 0022 で確立済み（span 親子写像の直接根拠）。
- gen_ai semconv は v1.37.0 に pin。本 slice 着手時に意図的 bump を検討する（ADR 0023 決定 6。自動追従しない）。
- ADR 0023 の 7 決定（採用ポリシー / signal 範囲 / 写像層集中 / `evorch.*` 骨格 / slice 二段分割 / semconv 追従 / 検証）は実装判断の Source of Truth として確定済み。

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/event-bus/`

Target part: 写像層の span 化拡張 + span OTLP exporter

## In Scope

- 写像層（`crates/event-bus/` 内）の span 化拡張: 内部 event → span 区間化（request / agent_run span）、task_id / agent_run_id 相関と ADR 0022 の addressing による委譲親子の span 親子（parent span）写像、`gen_ai.*` 標準属性（`invoke_agent` / `chat` / `execute_tool` 階層）の付与。
- `evorch.*` span 属性で session / task / agent_run ID を保持（metrics には載せない）。構造軸は低カーディナリティ列挙（`evorch.delegation.depth` / `evorch.delegation.role`）に留める。
- span OTLP exporter 追加（slice 1 で導入される feature gate に従い opt-in 既定 off）。
- sampling hook と ADR 0012 ハード上限（件数/時間/byte 閾値）の span 適用。
- 検証の span 拡張: golden/snapshot fixture（span 親子構造、pin 固定）、CI cardinality guard、debug exporter 経由の最小 OTLP E2E（span）1 本。

## Out Of Scope

- metrics 面の変更（slice 1 の責務。回帰させないことは review 観点）。
- span 可視化 pane の実装（gui-workbench 側の別 slice の責務）。
- raw の LLM I/O / SSE body / message 本文の収集・export（ADR 0012 の raw 非永続ポリシーに基づく恒久対象外）。
- cost / 料金計算指標の変更。
- semconv pin（v1.37.0）からの無断 bump（意図的 bump 手続き必須）。
- 内部 event schema の OTel 標準名への侵食・producer 層による opentelemetry crate の直接参照。
- feature gate の既定 on 化・強制有効化。

## Standalone Child Issue Contract

`turtton/evorch` の `crates/event-bus/` で、slice 1（v02-otel-metrics-exporter）が導入した写像層を span に拡張し、内部 event（ドメイン語彙保持）から委譲親子階層（task/run span → agent 実行 span(`invoke_agent`) → child `chat` → `execute_tool`）を task_id / agent_run_id 相関と ADR 0022 の親子 addressing に基づき span 親子として生成し、slice 1 で導入される feature gate（opt-in 既定 off）経路で OTLP span exporter から送出する。span 属性は OTel GenAI semconv（pin: 現基準 v1.37.0、着手時に意図的 bump を検討）に準拠させ、`evorch.*` span 属性で session / task / agent_run ID を保持する（metrics 側には載せない規律維持）。raw の LLM I/O・SSE 本文・message 本文・credential は span に含めない。sampling hook と ADR 0012 ハード上限を span 経路にも適用し、検証として pin 固定 fixture による golden/snapshot test（span 親子構造）、CI cardinality guard（ID 混入の静的ブロック）、debug exporter 経由の最小 OTLP E2E（span）1 本を入れる。metrics 面の変更・可視化 pane 実装・raw log 収集は行わない。

## Acceptance Criteria

- 委譲親子が span 親子（parent span 紐付け）として写像される（task/run span → agent 実行 span → `invoke_agent` → child `chat` → `execute_tool` の階層）。
- span 命名・属性が OTel GenAI semconv（`gen_ai.*`）registry に準拠する（pin 対象 semconv リリースを写像表に明記）。
- `evorch.*` span 属性に session / task / agent_run ID を保持し、metrics 側には載せない高カーディナリティ規律を維持する。
- raw の LLM I/O・SSE 本文・message 本文・credential を span 属性・span event として含めない。
- sampling hook と ADR 0012 ハード上限（件数/時間/byte 閾値）が span 経路に実効する。
- slice 1 で導入された写像層の golden/snapshot test（pin 固定 fixture）を span 親子構造へ拡張し、CI cardinality guard（ID 混入の静的ブロック）が span 属性に対しても効く。
- debug exporter 経由の最小 OTLP E2E（span）が 1 本入り、wire レベル疎通を assertion できる。

## Verification

- `cargo test -p event-bus`: 委譲 fixture の span 親子生成（golden/snapshot、pin 固定）、raw payload / credential 非混入の fixture 検証、sampling hook 既定値とハード上限の適用確認、feature off 時の既存計測・metrics export 回帰なし。
- CI cardinality guard による span 属性への ID 混入静的ブロック（metrics whitelist も形骸化させない）。
- 写像層 → OTel SDK exporter → debug exporter の JSON 出力 assertion による最小 OTLP E2E（span）1 本。
- `cargo clippy -p event-bus -- -D warnings` / `cargo fmt --check` / `git diff --check`。

## Related Links

- [diagnostics-self-improvement/overview.md](../../../intents/evorch/features/diagnostics-self-improvement/overview.md) — primary intent（委譲計測・観測は自己改善機能の分析基盤）
- [mvp-roadmap.md](../../../intents/evorch/technology/mvp-roadmap.md) — v0.2「役割の深化と観測」フェーズ
- [gui-workbench/overview.md](../../../intents/evorch/features/gui-workbench/overview.md) — span 可視化 pane は別 slice の責務
- [architecture.md](../../../intents/evorch/technology/architecture.md) — Agent Kernel 構成と crate 分割
- [0012-metrics-architecture.md](../../../intents/evorch/decisions/0012-metrics-architecture.md) — raw 非永続 / ハード上限 / sampling 方針
- [0014-config-architecture.md](../../../intents/evorch/decisions/0014-config-architecture.md) — feature gate の config 構成
- [0015-verification-two-layer.md](../../../intents/evorch/decisions/0015-verification-two-layer.md) — verification two-layer
- [0017-event-bus-transport.md](../../../intents/evorch/decisions/0017-event-bus-transport.md) — event bus broadcast 基盤
- [0019-runtime-reload-semantics.md](../../../intents/evorch/decisions/0019-runtime-reload-semantics.md) — exporter 設定の reload 枠組み
- [0022-parent-child-tree-addressing-and-nested-delegation.md](../../../intents/evorch/decisions/0022-parent-child-tree-addressing-and-nested-delegation.md) — 委譲親子 = span 親子の addressing 根拠
- [0023-otel-attribute-conventions.md](../../../intents/evorch/decisions/0023-otel-attribute-conventions.md) — 採用ポリシーと 7 決定の Source of Truth

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: primary = diagnostics-self-improvement、supporting = mvp-roadmap / gui-workbench。新規 intent node 不要（独立 OTel intent は作らない。slice 1 と同一 placement の span 側拡張）。
- ADR candidate: なし（条件付き decline。ADR 0023 は確定済み前提。本 slice 中に semconv pin を bump した場合のみ ADR 0023 change-log 追記）
- Diagram candidate: なし
- Docs update: `intents/evorch/technology/mvp-roadmap.md` の v0.2「役割の深化と観測」に OTLP exporter（span 側まで含む）完了を反映
- Closeout writeback expected: yes（mvp-roadmap 反映 ＋ bump 時のみ ADR 0023 change-log）。

## Guide Reachability (G645)

本 slice は kernel 側の export 面のみを扱い、role-facing surface を追加しない（span の可視化 pane は gui-workbench 側の別 slice の責務であり、本 slice の責務外。`no_role_facing_surface: true`）。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
