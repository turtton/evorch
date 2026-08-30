# v01-storage-maintenance-completion Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- `incremental_vacuum` がauto_vacuum前提を正しく扱い、既存DBに起動時full VACUUM等の無制限blockingを課さないこと。
- vacuumが明示threshold成立時のみ、設定page budget以内で実行され、一tickで全freelistを掃除しないこと。
- single-writer maintenance loop内で実行され、別Connection/writerを増やしてADR 0018を破らないこと。
- temp測定範囲がevorch/SQLite管理対象として明確で、OS全体 `/tmp` を誤って合算しないこと。
- typed diagnosticが超過/復帰の状態遷移でemitされ、毎tick warning stormにならないこと。
- diagnostic/tracingに不要なpath/content/secretを含めず、bytes/threshold/page count等の運用情報を持つこと。
- freelist fixtureがbudgeted段階回収を、temp fixtureが未満/超過/復帰を実証すること。
- retention/downsampling/schema/GUIへscopeを広げず、既存checkpoint/hard-limit動作を維持すること。

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

`closeout_learning.write_back_required` は `true`。`intents/evorch/features/storage-memory/overview.md` にauto_vacuum既存DB方針、vacuum trigger/page budget、temp測定範囲/threshold、typed diagnosticの超過・復帰state transitionが実装値として記録されていることを確認する。
