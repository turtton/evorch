# v01-provider-compatible-usage-contract Implementation Packet

## Goal

OpenAI-compatible endpoint の usage accounting が、streaming / non-streaming と retry/fallback の全完了経路で exactly once になることを direct contract test で固定し、「完了した provider attempt が usage を1回だけ発行し、coordinator は再発行しない」という所有権を文書化する。

## Why

v0.1 inspect の slice #4 で低 severity の検証 gap が見つかった。実装は non-stream で response 変換後に1回（`crates/providers/src/provider/openai.rs:136-171`）、stream で completion signal 時に1回（`crates/providers/src/http/stream.rs:156-171`）usage を発行する。しかし OpenAI-compatible の統合テストは non-stream の1件を読むだけで二件目不在を確認せず、stream test は EventBus を接続していない（`crates/providers/tests/openai_compatible_contract.rs:34-102`）。さらに routing は fallback 候補を返すだけ（`crates/routing/src/router.rs:150-218`）で、retry/fallback across attempts の accounting contract が直接証明されていない。v0.1.1 ではロジックを広げず、将来の coordinator 接続でも二重計上しない契約を先に固定する。

## Scope

- `crates/providers/tests/openai_compatible_contract.rs:34-102` を拡張し、non-stream / stream の各 completed request で正しい provider label / model / token fields の UsageEvent がちょうど1件、二件目が無いことを wiremock で検証する。
- streaming では複数 content delta、usage-bearing frame、DONE、同一chunk内の完了後frame、tail completion を含む fixture で `SsePump::absorb_interpretation`（`crates/providers/src/http/stream.rs:156-171`）の一回性を end-to-end に固定する。
- HTTP 4xx/5xx、timeout/transport、invalid JSON/SSE、completion signal 無し、consumer が Completed 前に stream をdropする経路で UsageEvent が0件であることを検証する。
- retry/fallback scenario 用の test harness を provider/routing の既存境界に置く。失敗 attempt は0件、最終 completed attempt は1件、logical request 合計も1件であること、勝者 profile label / model だけが記録されることを確認する。
- coordinator が usage event を再emitせず、provider attempt completion path（non-stream response conversion / stream Completed）が唯一の発行所有者であることを `UsageEmitter` / client rustdoc と provider-routing overview に明記する。
- 共通 `ChatCompletionsClient` を使う OpenAI と OpenAI-compatible の回帰、および Anthropic の既存 usage tests を維持する。

## Out of scope

- routing/fallback candidate order、retry 回数、cooldown / affinity policy の変更。
- 新 provider / protocol、UsageEvent schema 変更、aggregator / storage semantics 変更。
- provider request observation schema / TTFT（別 packet `v01-event-provider-observation-schema`）。
- 実 API / credential を使うテスト。

## Verification

- `cargo test -p providers --test openai_compatible_contract`：non-stream / stream success の exactly-one、error/drop の zero、provider/model/token payload を検証する。
- `cargo test -p providers`：OpenAI / Anthropic / shared SSE pump の usage tests を含む全 provider 回帰。
- `cargo test -p routing`：fallback candidate order / affinity の既存挙動と、追加した retry/fallback accounting harness を検証する。
- event receiver の二件目不在は bounded timeout で確認し、request mock の `.expect(1)` で HTTP attempt 数も固定する。
- `cargo check --workspace` と `git diff --check`。

## 実装確定（2026-08-30、PR #40 / issue #39）

usage 発行の exactly-once 契約がテストと rustdoc で pin された。要点:

- **src 差分は doc のみ**（挙動変更なし）。`UsageEmitter` / `SsePump` / `adapt_sse_stream` / `ChatCompletionsClient::{send,stream}` / `Router::next_fallback` に発行所有権の rustdoc を明文化: 完了した attempt のみ 1 回発行、失敗経路 0 件、コーディネータは発行・再発行しない
- **providers 契約テスト**（tests/openai_compatible_contract.rs、13 ケース相当）: send 成功強化（二件目不在）/ stream exactly-once ×2（複数 usage frame・post-DONE frame・二重 [DONE]、Started→FirstToken→Usage→Completed の wire 順序も固定）/ 失敗 0 件 ×8 種（HTTP 500/timeout/invalid JSON × send・HTTP 500/invalid JSON frame/invalid SSE/completion 不落 EOF/consumer drop × stream）。constexpr fixture DUPLICATE_USAGE_SSE / NO_COMPLETION_SIGNAL_SSE
- **provider/routing 境界 harness**（crates/routing/tests/fallback_usage_contract.rs）: run_send_with_fallback / run_stream_with_fallback を coordinator として直接実装し × 4 test（fallback send 勝者 1 件・同一 profile retry 勝者のみ・枯渇 0 件・fallback stream 勝者 1 件）。wiremock `.expect(1)` で HTTP attempt 数を固定、勝者 profile label/model を確認
- routing dev-deps に wiremock / futures-util / tokio rt-multi-thread を追加（すべて workspace 既存管理・PR body 開示済み。Cargo.lock に新規外部 package なし）

## Knowledge Maintenance (G461, optional)

- Intent placement: provider-routing を primary、ADR 0004 を supporting とする。新規 node は不要。
- ADR candidate: decline。既存 provider/profile/protocol 分離のテスト保証であり新規 decision はない。
- Diagram candidate: decline。topology/state transition は変えない。
- Docs update: required。provider-routing overview に completed attempt / failed attempt / coordinator の usage 発行所有権と exactly-once 保証を記録する。
- Closeout learning: `write_back_required: true`。stream/non-stream/retry/fallback/drop の境界表と勝者 label semantics を書き戻す。
- Guide reachability (G645): `no_role_facing_surface: true`。内部 contract tests と accounting guarantee の追加のみ。

`improve` (G456 / G460) は後続の安全網。本 packet では実装所有権と tests を同時に固定する。
