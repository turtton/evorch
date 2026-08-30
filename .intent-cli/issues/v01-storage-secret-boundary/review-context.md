# v01-storage-secret-boundary Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- guardがmessage/eventの正規writer ingressで必ず実行され、低水準repo経路が残る場合も迂回不能であること。
- known credential valueの収集が限定的・test可能で、全環境変数scanや値の長期Debug保持をしないこと。
- API-key-shaped ruleが高signalで、一般的な長文/UUID/model idを安易に拒否しないこと。
- rejectがSQL mutation、event serialize/accounting更新より前でDBを不変にすること。
- `StorageError`、tracing、assert message、Debugにsecret候補や前後contextを含めないこと。
- positive/negative/redaction testsが値本体をfixture failure outputへ漏らさないこと。
- 実装/docsが「heuristic defense-in-depthで完全保証ではない」と明記し、過大なsecurity claimをしないこと。
- provider/keychain/config secret rejectionや既存DBscanへscopeを広げていないこと。

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

`closeout_learning.write_back_required` は `true`。`intents/evorch/features/storage-memory/overview.md` に対象text field、known-value/API-shape検出、拒否とredacted診断、false-positive対策、完全保証ではない限界が記録されていることを確認する。
