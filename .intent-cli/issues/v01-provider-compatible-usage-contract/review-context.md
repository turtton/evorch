# v01-provider-compatible-usage-contract Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- non-stream と stream の completed request ごとに UsageEvent が正確に1件で、receiver の二件目不在まで検証しているか。
- HTTP/transport/parse error、completion signal 無し、Completed 前の consumer drop で usage が0件か。部分streamをcompleted usageとして数えていないか。
- retry/fallback harness が失敗attemptごとの0件と勝者attemptの1件を区別し、logical request全体でも1件に保つか。
- usage event の provider label / model / token fields が最終勝者attempt由来で、敗者profileやlogical model名に置換されていないか。
- provider completion pathだけが発行し、routing/coordinator/aggregatorが同じusageを再emitしない明確な所有権になっているか。dedupeをtimeout等の偶然に頼っていないか。
- wiremockのrequest `.expect(1)`、複数delta/usage/DONE/tail fixtures、bounded second-event timeoutでattempt数とevent数を直接検証しているか。
- 共通 ChatCompletionsClient の変更がOpenAIを壊さず、Anthropic既存usage testsも通るか。routing order/retry policy/UsageEvent schemaにscope creepしていないか。

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

本 packet は `closeout_learning.write_back_required: true`。`intents/evorch/features/provider-routing/overview.md` に、completed attempt の定義、stream/non-stream/error/drop/retry/fallback のevent件数表、provider completion pathが唯一のusage発行所有者でcoordinatorは再発行しない保証、勝者label semanticsが書き戻されていることを確認する。新規ADR / diagram / intent nodeは不要。

## Guide Reachability (G645)

本 packet は `no_role_facing_surface: true`。内部契約テストとaccounting保証以外のguide routeを追加していないことを確認する。
