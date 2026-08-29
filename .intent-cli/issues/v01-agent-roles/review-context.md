# v01-agent-roles Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific focus

- Role の capability boundary が runtime レベルで強制されているか（Orchestrator に mutation tool が渡らない、Explorer に write / edit / delegate が無い）。config での緩和が builtin の不可侵部分を越えていないかを確認する
- AgentRun の状態遷移が event stream へ emit され、background の開始・完了・キャンセルが観測できること
- role → model routing を本 packet 内で自前実装していないこと（v01-routing-profiles への委譲）
- 複数 AgentRun の context 独立性（一方への変更が他方に漏れない）がテストで検証されていること

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

本 packet は `closeout_learning.write_back_required: false` であり、intent-tree / ADR / diagram / docs の必須書き戻しは無い（decline）。closeout で 4 role の capability 設定値の実測結果と AgentRun 状態遷移の確定パターンが学習として回収されることだけ期待する。必須書き戻しが無いこと自体はブロッキングにしない。