# v02-otel-span-exporter Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

Particular focus for this slice.

- slice 1（v02-otel-metrics-exporter）が `crates/event-bus/` に導入した写像層・metrics exporter に手を入れる際に metrics 面を回帰させていないか。① で入る既存の golden/snapshot test・cardinality guard・metrics OTLP E2E がそのまま通ることを確認する。
- span 親子付けの破綻がないか。event schema の task_id / agent_run_id 相関と ADR 0022 の親子 addressing に基づく ID 受け渡し（orchestrator/run/task）に漏れがなく、区間化漏れで孤立 span が生えていないかを重点確認する。
- span 属性に raw payload（LLM I/O / SSE 本文 / message 本文）や credential が混入していないか（ADR 0012 の raw 非永続ポリシーとプライバシー方針）。
- sampling hook の既定値と ADR 0012 のハード上限（件数/時間/byte 閾値）が span 経路に実効しているか。switch 文（on/off）や設定解釈のミスで無制限 export になっていないか。
- slice 1 で導入される feature gate が既定 off のまま維持されているか（有効化は ADR 0014 の設定経路のみ。feature off でのコンパイル・通常起動に影響しないこと）。
- ID 系属性の metrics 混入を拒む CI cardinality guard が、span 属性の追加によって形骸化していないか。
- semconv は pin（現基準 v1.37.0）前提。無断 latest 追従や、bump する場合の「release 差分精読 → 写像表差分 → 検証結果」の明示表抜きの仕様変更でないこと。

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

`closeout_learning.write_back_required` は `true`。`intents/evorch/technology/mvp-roadmap.md` の v0.2「役割の深化と観測」への OTLP exporter（span 側まで含む）完了反映が本 PR または明示的な follow-up に含まれることを確認する。加えて、本 slice で semconv pin を bump した場合のみ ADR 0023 への change-log 追記（bump 手続きの明示表）が条件付き対象。bump 非実施なら ADR 0023 の更新は不要。それ以外の knowledge maintenance は decline であり、ブロッカーとせず事実として記録する。
