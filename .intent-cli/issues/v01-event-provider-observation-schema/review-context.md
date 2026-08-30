# v01-event-provider-observation-schema Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- request ID が start / TTFT / completed/error / fallback を一意に相関し、retry/fallback の別 attempt を同一 attempt と混同していないか。
- TTFT が HTTP headers 到着ではなく最初の user-visible text/tool delta 解釈時であり、keepalive・usage-only・空・reasoning-only delta を除外して高々1回 emit するか。
- streaming の completion 無し、invalid SSE/JSON、transport error、consumer drop/cancel に終端 semantics があり、start だけ残る silent path がないか。
- completed event と既存 UsageEvent の token counts が二重集計されない明確な責務・correlation contract になっているか。
- provider/profile/protocol/model/failure の payload が診断に十分で、API key・prompt・response 本文など機密データを含まないか。
- OpenAI / Anthropic / OpenAI-compatible の共通 HTTP/SSE path で emission が一貫し、wiremock + deterministic time test が順序と duration を非 flaky に検証するか。
- `Router::next_fallback` の候補順序・affinity semantics や retry policyを変更せず、観測だけを追加しているか。metrics downsampling / storage に踏み込んでいないか。

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

本 packet は `closeout_learning.write_back_required: true`。`intents/evorch/features/provider-routing/overview.md` と `intents/evorch/features/diagnostics-self-improvement/overview.md` に、attempt lifecycle、request ID correlation、TTFT first-token 判定、stream error/drop semantics、UsageEvent との accounting 関係が書き戻されていることを確認する。新規 ADR / diagram は不要。

## Guide Reachability (G645)

本 packet は `no_role_facing_surface: true`。内部 schema / emission の変更に role-facing route が追加されていないことを確認する。
