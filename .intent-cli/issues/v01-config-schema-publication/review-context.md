# v01-config-schema-publication Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- schemaがhandwritten duplicateではなく `Config` + `schemars::schema_for!` から生成されること。
- artifact pathがversionedかつstableで、`CURRENT_VERSION=2` とfilename/metadataが一致すること。
- generator出力が決定的で、同じtreeから再生成して差分ゼロになること。
- CIが実際に再生成結果をchecked-in artifactと比較し、単なるJSON parse testで終わっていないこと。
- providers/routing/panel/diagnostics/permissions/metricsとcredential reference/enumをschemaが反映すること。
- config-referenceのリンクと再生成commandが実在path/commandに一致すること。
- parsing、migration、unknown-key許容、secret-field policyを変更してscopeを広げていないこと。

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

`closeout_learning.write_back_required` は `true`。`intents/evorch/operations/config-reference.md` のJSON Schema節が、実在するv2 artifact path、決定的な再生成command、エディタでの利用方法、CI同期方針を記載していることを確認する。新規ADRやintent nodeは不要。
