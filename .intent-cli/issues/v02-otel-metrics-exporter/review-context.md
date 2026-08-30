# v02-otel-metrics-exporter Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

Particular focus for this slice.

- span / trace・span 属性・cost inspector 計算変更・GUI / TUI surface は review 対象外として flag すること（本 slice の scope 蔓延禁止）。これらは slice ②（`v02-otel-span-exporter`。本 slice に依存する側）の範囲である。
- raw LLM I/O log / SSE body / message 本文の export 実装が混入していないこと（ADR 0012 の raw 非永続ポリシーにより恒久対象外）。
- producer（provider / runtime / tool 層）が opentelemetry crate を直接 import していないこと。OTel 属性名の知識は crates/event-bus 内の写像層 module に集中していることを確認する。
- metrics label に ID 系（request ID / run 識別子等）・自由文字列が混入していないこと。メトリクス属性は ADR 0023 決定 4 の whitelist（集計軸 session / task / agent_run / profile ＋ 低カーディナリティ構造軸 `evorch.delegation.depth` / `evorch.delegation.role`）内に収まっていることを、コードと CI cardinality guard 双方で確認する。
- `evorch.*` に追加された属性がすべて ADR 0023 決定 4 の whitelist と整合していること。whitelist 外の構造軸（branch 等）の追加は本 slice で行わない。
- 新規導入の feature flag `otel-exporter` が opt-in 既定 off であり、feature off（既定）ビルドで既存の event 配信・bus 動作が回帰していないこと。
- gen_ai semconv pin（v1.37.0）からの逸脱（新しい属性名・well-known 値への追従）が本 slice に混入していないこと。

## Facet context

<!-- BEGIN GENERATED FACET CONTEXT (G530) -->
### vocabulary
- (none overlapping this packet's intent_references)
### invariant
- (none overlapping this packet's intent_references)
### decider
- (none overlapping this packet's intent_references)
### acceptance-property
- (none overlapping this packet's intent_references)
<!-- END GENERATED FACET CONTEXT (G530) -->

## Knowledge Writeback Expectation (G461)

本 PR（または明示 follow-up）に ADR 0023 確定版（決定 4 の `evorch.*` whitelist の最終反映）と mvp-roadmap.md の v0.2「役割の深化と観測」における OTLP exporter（metrics）着手・完了の反映が含まれることを確認する。`closeout_learning.write_back_required` は `true` であり、この packet の acceptance criteria 最後の項目でもあるため省略は不可。
