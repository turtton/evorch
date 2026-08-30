## Goal

provider request attempt の start/end、TTFT、token counts、error、fallback trigger を typed EventBus schema として定義し、OpenAI / Anthropic / OpenAI-compatible client から相関可能に emit する。

## Why This Slice Exists Now

v0.1 inspect の slice #2 は MINOR_DRIFT。`v01-event-stream` が要求した provider request-response / TTFT contract に対し、現行 `ProviderEvent` は `ProviderFallback` しか持たない（`crates/event-bus/src/event.rs:265-278`）。usage event は存在するが（同:233-263）、attempt start・first token・error と同一 request として追跡できない。provider 3実装と EventBus transport が確定済みのため、v0.1.1 で diagnostics / metrics が消費できる観測 schema を閉じる。

## Current Observed State

- Event metadata は schema_version / monotonic / wall-clock を持つ（`crates/event-bus/src/event.rs:9-24`）。
- EventKind には Provider category がある（`crates/event-bus/src/event.rs:60-76`）が、ProviderEvent は fallback だけで request lifecycle を表さない（同:265-278）。
- UsageEmitter は non-stream response 変換後と stream completion 時に UsageEvent を発行する（`crates/providers/src/provider/openai.rs:140-170`; `crates/providers/src/http/stream.rs:156-171`）。
- OpenAI-compatible は OpenAI と同じ `ChatCompletionsClient` を利用する（`crates/providers/src/provider/openai_compatible.rs:19-78`）。
- routing の `next_fallback` は候補選択と affinity 再pinだけを行い、provider request retry 自体は実行しない（`crates/routing/src/router.rs:150-218`）。

## Accepted Baseline You May Assume

- Rust edition 2024 / rust-version 1.97、Tokio 1、serde 1、reqwest 0.12、tracing 0.1。
- Event schema は serde `kind` / `payload` 隣接タグと schema_version 1。
- OpenAI / Anthropic / OpenAI-compatible の ProviderClient send / stream と wiremock 契約テストは実装済み。
- UsageEvent と UsageEmitter は既存 token accounting surface として維持する。
- ADR 0017 により transport は in-process Tokio broadcast、拡張ゲートは schema_version と確定済み。

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/event-bus/`, `crates/providers/`, `crates/routing/`

Target part: typed provider observation schema、provider client / fallback emission、TTFT measurement contract。

## In Scope

- request start / completed / TTFT / error / fallback triggered の typed variants と request ID correlation。
- provider/profile/protocol/model、duration、token counts、typed failure reason の必要最小 payload。
- 3 provider clients の streaming / non-streaming attempt emission。
- first observable content/tool delta を基準とする TTFT 定義と一回性。
- routing fallback selection との観測接続（policy/order は不変）。
- serde / wiremock / deterministic time tests と intent docs writeback。

## Out Of Scope

- metrics downsampling / storage retention。
- 新 provider / protocol。
- retry/fallback order、cooldown、affinity policy の変更。
- prompt / response content や credential の event 記録。

## Standalone Child Issue Contract

`crates/event-bus` に provider attempt の typed observation events を追加し、`crates/providers` の OpenAI / Anthropic / OpenAI-compatible send / stream から request start、first observable token、completed/error を同一 request ID で emit してください。TTFT は送信直前から最初の user-visible text/tool delta 解釈時までとし、headers・keepalive・usage-only・空・reasoning-only delta は除外します。fallback selection も from/to profile/model/failure と attempt correlation を emit しますが、routing/retry policy は変更しません。serde と wiremock の契約テストで順序・一回性・payload を固定し、定義を intent docs へ書き戻してください。

## Acceptance Criteria

- [ ] request start / completed / TTFT / error / fallback triggered の typed ProviderEvent が request ID で相関できる。
- [ ] TTFT の開始点・終了点・除外対象が rustdoc と intent docs に明記され、attempt ごとに高々1回である。
- [ ] OpenAI / Anthropic / OpenAI-compatible の send / stream が成功・HTTP error・timeout/transport・invalid response の start と終端 event を emit する。
- [ ] completed の token counts が既存 UsageEvent と明確に相関し、二重計上を誘発しない契約になっている。
- [ ] fallback event が from/to profile/model/failure/request correlation を持ち、既存 candidate order / policy は変わらない。
- [ ] 全追加 variant が serde round-trip と schema version test を通る。
- [ ] wiremock + deterministic time tests が event order、TTFT 一回性、request correlation、token/error/fallback payload を検証する。
- [ ] downsampling、storage、新 provider、routing policy 変更を含めない。

## Verification

- `cargo test -p event-bus`
- `cargo test -p providers`
- `cargo test -p routing`
- `cargo check --workspace`
- `git diff --check`

## Related Links

- `intents/evorch/features/provider-routing/overview.md:5-21,41-45`
- `intents/evorch/features/diagnostics-self-improvement/overview.md:9-21`
- `intents/evorch/decisions/0004-provider-routing-separation.md:11-36`
- `intents/evorch/decisions/0017-event-bus-transport.md:11-30`
- v0.1 slice #2 / issue #2 (`v01-event-stream`)
- v0.1 issue #4 (`v01-provider-client`)

## Knowledge Maintenance

- Intent placement: provider-routing（primary）、diagnostics / ADR 0004 / ADR 0017（supporting）。新規 node なし。
- ADR candidate: none。既存 ADR の具体化。
- Diagram candidate: none。
- Docs update: provider-routing / diagnostics overview に event sequence と TTFT 定義を追記。
- Closeout writeback expected: yes。

## Guide Reachability (G645)

`no_role_facing_surface: true`。内部 event schema と provider emission の追加であり、role-facing command / UI / guide target は追加しない。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
