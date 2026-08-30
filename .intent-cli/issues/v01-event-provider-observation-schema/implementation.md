# v01-event-provider-observation-schema Implementation Packet

## Goal

event-bus に provider request attempt の typed observation schema を追加し、OpenAI / Anthropic / OpenAI-compatible client と routing fallback 境界から request start/end、TTFT、token counts、error、fallback trigger を相関可能な event として発行する。TTFT の first-token 判定を曖昧さなく文書化し、wiremock 契約テストで event sequence を固定する。

## Why

v0.1 inspect の slice #2 は MINOR_DRIFT だった。元の `v01-event-stream` packet は provider request-response と TTFT timestamp を要求していたが、現行 `ProviderEvent` は fallback variant 1つだけである（`crates/event-bus/src/event.rs:265-278`）。usage は完了時に発行される一方（`crates/providers/src/http.rs:114-150`）、request attempt の開始、first token、失敗、fallback との相関がなく、diagnostic timeline と TTFT 自動集計の入力契約が完成していない。provider client 3実装と in-process versioned EventBus が確定した今、storage downsampling より先に観測語彙を固定する。

## Scope

- `crates/event-bus/src/event.rs:60-76,233-278` の EventKind / UsageEvent / ProviderEvent を拡張し、request ID で相関できる typed provider attempt events を定義する。
- request start、request completed、TTFT、request error、fallback triggered の payload に provider profile / protocol / model / request ID / outcome / duration / token counts / typed failure reason のうち各 event に必要な情報を持たせる。API key、prompt 本文、response 本文は含めない。
- TTFT は「HTTP request attempt の送信直前」から「wire stream の最初の user-visible text delta または tool-call delta を正常解釈した瞬間」までとする。headers 到着、keepalive、usage-only frame、空 delta、reasoning-only delta は除外し、first observable delta ごとではなく request attempt ごとに高々1回発行する。
- `crates/providers/src/provider/openai.rs:140-210`、`provider/anthropic.rs:84-139`、`provider/openai_compatible.rs:48-78` の send / stream に共通 observation emitter / attempt context を接続する。成功だけでなく HTTP status、timeout/transport、invalid JSON/SSE、stream completion 無しも終端 event へ写す。
- `crates/providers/src/http/stream.rs:62-181` の SSE pump で first observable delta と完了/error の境界を一箇所に集約し、stream object の drop/cancel を契約上どう扱うかを明記・テストする。
- `crates/routing/src/router.rs:150-218` は fallback 候補を選ぶだけで provider call retry を実行しない。この境界から selection/fallback event を発行するか、実 attempt executor が別に必要なら既存責務を崩さない最小 adapter を置く。候補順序は変えない。
- event-bus serde table test と wiremock provider contract tests で、event order、request ID、TTFT 一回性、token counts、error/fallback payload を固定する。

## Out of scope

- raw metrics の downsampling、ring-buffer / storage policy、長期 metrics retention。
- 新しい provider / protocol、credential flow。
- routing candidate order、retry 回数、cooldown / affinity policy の変更。
- prompt / response 本文の event payload 化、機密情報の記録。

## Verification

- `cargo test -p event-bus`：追加 ProviderEvent 全 variant の serde round-trip、schema_version、timestamp fields。
- `cargo test -p providers`：OpenAI / Anthropic / OpenAI-compatible の streaming / non-streaming 成功・HTTP error・timeout/transport・invalid response について start → TTFT（stream successのみ、高々1回）→ completed/error の順序と request ID 相関を wiremock で検証する。
- `cargo test -p routing`：既存 fallback order を保持し、fallback observation payload が from/to profile/model/failure を正しく表す。
- deterministic/paused Tokio time または注入 clock で TTFT duration を検証し、wall-clock の sleep に依存する flaky test を作らない。
- `cargo check --workspace` と `git diff --check`。

## Knowledge Maintenance (G461, optional)

- Intent placement: provider-routing を primary、diagnostics-self-improvement / ADR 0004 / ADR 0017 を supporting とする。新規 node は不要。
- ADR candidate: decline。transport/versioning と routing separation は既存 ADR で決定済み。
- Diagram candidate: decline。新規 topology はなく、event sequence は docs の短い sequence 記述で十分。
- Docs update: required。provider-routing と diagnostics overview に attempt event sequence、request correlation、TTFT first-token 定義を記録する。
- Closeout learning: `write_back_required: true`。3 client で共通化した emission boundary、stream drop/error semantics、UsageEvent との accounting 関係を書き戻す。
- Guide reachability (G645): `no_role_facing_surface: true`。内部 typed schema / emission の追加であり、role が直接操作する新 surface は増えない。

`improve` (G456 / G460) は後続の安全網であり、本 packet の measurement contract は closeout 時に必ず intent へ固定する。
