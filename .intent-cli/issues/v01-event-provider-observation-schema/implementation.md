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

## 実装確定（2026-08-30、PR #32 / issue #31）

- **発行境界の共通化**: crates/providers/src/observe.rs の `AttemptObserver` が 3 クライアント共通の発行境界。wire request 構築成功直後・HTTP 送信直前に `emit_started()`（この呼出時点に計測時計を合わせる）。終端（Completed/Failed）は `terminal_emitted` flag でちょうど 1 回、consumer drop 等の未終端は `Drop` で `Other` 失敗として終端化（reviewer gate で検出された修正）
- **SSE 観測**: SsePump が全失敗入口（InvalidSse / transport / interpret / finish 時 SSE・interpret / 中途 EOF）で `emit_failed` を保証。中途 EOF は canonical ストリームには Err を流さず従来契約のまま、bus にのみ Transport 失敗を 1 件発行
- **request ID**: `req-<process 起動 ms>-<process 内単調 counter>`（observe.rs の LazyLock+AtomicU64）。プロセス再起動跨ぎは起動時刻で実用上回避
- **TTFT first-token 判定**: `note_delta` が canonical `StreamEvent` を観測し、非空 TextDelta か ToolCallDelta の最初の 1 回のみ発行（ReasoningDelta/空 text/Completed は除外）。tokio::time::Instant 基準で paused time テストが時間を厳密検証
- **token accounting**: `RequestCompleted` は UsageEvent::Usage の値をそのまま観測複製として保持。canonical 集計は UsageEvent のみ、bus 順序（Started→Usage→Completed）で相関。UsageEvent 自体に request ID は入れない（wire 不変制約）
- **fallback 観測**: `Router::next_fallback` の候補選択境界で `FallbackTriggered` を発行（`with_event_bus` 接続時のみ。候補枯渇では非発行）。routing の FailureKind → ProviderFailureKind 写像は HTTP status を落とす（routing 層で保持しない設計）。`Router` の derive(PartialEq) は event_bus を除外する手動 impl に変更
- **依存追加の内訳**: routing に event-bus 内部 path 依存（FallbackTriggered payload のため、packet target_path で許容）＋ dev tokio[macros,rt,time]。providers は dev のみ tokio test-util を有効化。新規外部 dep なし
- **検証実績**: cargo fmt/check/clippy --workspace --all-targets -D warnings 緑、test 全 67 suites 0 fail（wiremock 契約テスト 3 client 成功・timeout・HTTP500・不正 JSON/SSE・consumer drop・fallback 発行/枯渇、serde round-trip・legacy snapshot）、schema_version=1 不変
- 引き継ぎ: runtime 側で `with_profile` / `Router::with_event_bus` を接続する production 配線は後続 unit（diagnostics 連携）の責務
