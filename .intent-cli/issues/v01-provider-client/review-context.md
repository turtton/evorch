# v01-provider-client Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- 統一 `ProviderClient` trait が `send` / `stream` を提供し、3 provider がそれを実装しているか（v01-scaffold の crate 構成に従っているか）
- canonical message 正規化と OpenAI ↔ Anthropic 変換が round-trip で検証されているか
- ストリーミング parse が wiremock の recorded fixture で検証され、実 API へのアクセスがテストに含まれていないか（ADR 0015 第1層。CI は mock のみ）
- Usage が event stream へ usage イベントとして emit されるか（ADR 0012: tok/s・コスト計算の入力）
- credential を trait 内で保持していないか（保存は v01-sandbox-approval の責務。引数注入に留める）
- subscription 系 provider（openai-codex / github-copilot / anthropic-subscription）を実装していないか（v0.3 対象）
- typed error の網羅（4xx / 5xx / 429 / timeout / 不正 SSE）

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

注: `intent-cli intent facet-check` は lexically にしか確認しない（no_facet_data は語彙の一致有無を報告し、意味検証はしない）。上記 Slice-specific review focus が意味アライメントの主たる確認点である。

## Knowledge Writeback Expectation (G461)

`closeout_learning.write_back_required` は `true`。closeout で以下が記録されているか確認する（この PR 内 or follow-up packet として）。

- ADR 0016（統一 canonical message 正規化と OpenAI / Anthropic / OpenAI-compatible 変換）の新設
- `features/provider-routing/overview.md` への v0.1 3 provider 確定と Usage イベント記録
- `technology/mvp-roadmap.md` の Open question『v0.1 で用意する provider は3種で確定か』の解消記録

記録が未実施の場合は知識 writeback が不足している旨を review 所見に残す。