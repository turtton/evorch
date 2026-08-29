# v01-routing-profiles Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific focus

- config の優先順位（CLI > project > user > defaults）と `config.d/*.toml` の辞書順 deep merge（後勝ち）が ADR 0014 どおりに実装・テストされているか
- version migration が動作し、schemars による JSON Schema 生成が含まれているか
- model catalog が ADR 0013 のうち v0.1 分（builtin / fetch / /v1/models 検出・属性未確定）だけを実装し、subscription 動的フィルタ（v0.3）に踏み込んでいないか
- credential が config に書かれていないこと（参照のみ）
- テストがネットワーク / 実プロバイダに依存せず mock で完結していること

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

本 packet は `closeout_learning.write_back_required: false` だが、`knowledge_updates.docs.required: true` のため、ADR 0014 に従い `intents/evorch/operations/config-reference.md` の作成（および operations/ の新設）が closeout で実施されていることを確認する。intent-tree / ADR / diagram の書き戻しは無い（decline）。