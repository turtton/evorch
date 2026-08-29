## Goal

v0.1 用の provider 抽象を実装する。統一 `ProviderClient` trait（非同期 `send` / `stream`）と OpenAI / Anthropic / OpenAI-compatible の3実装を `crates/providers/` に追加する。メッセージは provider 非依存の canonical 形式に正規化し、wire 形式との相互変換を持つ。SSE ストリーミングを受信して delta を結合し、Usage を event stream へ emit する。

## Why This Slice Exists Now

mvp-roadmap の v0.1 で provider 3種が最小構成として確定しており、agent 実行の第一歩が provider 呼び出しなので、その抽象を最初に作る slice が必須である。ADR 0004 の provider type / API protocol 分離をコードで具体化し、ADR 0015 第1層（CI mock 契約テスト）を満たす最初の slice でもある。後続の routing / event-stream / context-engine がこの trait に依存する。

## Current Observed State

Greenfield 状態（Rust コードは未存在。`v01-scaffold` が crate 骨格を作る予定）。provider 呼び出しは一切できず、trait・変換・mock テストのいずれも存在しない。既存 harness は provider を直接結合しており、抽象化は未確立。

## Accepted Baseline You May Assume

- tech stack: Rust / Tokio / reqwest / serde（architecture.md）。tracing で計装
- ADR 0004: provider type / profile / logical model / API protocol の4層分離。API protocol は provider と独立
- ADR 0015 第1層: CI では mock provider + recorded response fixture のみを使用し、実 API へアクセスしない
- `v01-scaffold` が crates/ workspace と tracing 基盤を用意済み
- `v01-event-stream` が event stream（usage / tool_result 等）のイベント型と publish 経路を用意済み
- credential の保存・管理は本 slice の責務外。auth 情報は trait 呼び出しの引数で注入される
- subscription 系 provider（openai-codex / github-copilot / anthropic-subscription）は v0.3（re-evaluation-2026-08.md §1）

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/providers/`

Target part: provider trait・メッセージ変換・API クライアント

## In Scope

- 統一 `ProviderClient` trait（`send` / `stream`）と `ProviderCapabilities` の返却
- canonical Message 正規化と OpenAI ↔ Anthropic 相互変換（OpenAI-compatible は chat.completions wire を共用）
- reqwest による SSE ストリーミング（delta 結合、finish reason、error event）
- Usage parse → event stream への usage イベント emit（ADR 0012 の入力）
- typed error（4xx / 5xx / 429 + Retry-After / timeout / 不正 SSE）
- wiremock による mock 契約テスト（recorded response fixture）

## Out Of Scope

- subscription 系 provider（openai-codex / github-copilot / anthropic-subscription）— v0.3
- routing / fallback / session affinity / cooldown（v01-routing-profiles）
- credential 保存・設定・sandbox（v01-sandbox-approval）
- model catalog / コスト計算本体（ADR 0013 / ADR 0012。本 slice は usage イベントの供給源のみ）

## Standalone Child Issue Contract

`turtton/evorch` に `crates/providers/` 配下で、統一 `ProviderClient` trait（非同期 `send` と `stream` を提供、`ProviderCapabilities` を返す）と、OpenAI / Anthropic / OpenAI-compatible の3実装を追加する。メッセージは provider 非依存の canonical 形式へ正規化し、OpenAI chat.completions・Anthropic Messages API・OpenAI-compatible chat.completions の wire 形式との相互変換を実装する。reqwest で SSE ストリーミングを受信して delta を結合し、tool call を canonical 形式へ変換し、Usage（input / output / cache read / cache write）を event stream へ usage イベントとして emit する。HTTP 4xx/5xx / 429（Retry-After）/ timeout / 不正 SSE は typed error を返す。検証は wiremock による mock 契約テスト（recorded response fixture、ADR 0015 第1層）と canonical message round-trip テストで行い、実 API へはアクセスしない。subscription 系 provider は実装しない。PR は `main` をターゲットにする。

## Acceptance Criteria

- 統一 `ProviderClient` trait（`send` / `stream`）を OpenAI / Anthropic / OpenAI-compatible の3実装が実装し、`ProviderCapabilities` を返す
- canonical Message 正規化と wire 変換を持ち、OpenAI ↔ Anthropic の round-trip テストで検証できる
- mock HTTP server（wiremock、recorded response fixture）で SSE ストリーミング応答の parse・delta 結合が検証される（ADR 0015 第1層）
- tool call が provider 別 wire 形式から canonical tool_call へ変換される
- Usage（input / output / cache read / cache write token 数）が parse され、event stream へ usage イベントとして emit される
- HTTP 4xx/5xx・429（Retry-After）・timeout・不正 SSE に対して typed error を返す

## Verification

- `cargo test`: wiremock で 3 provider の `send` / `stream` を recorded fixture で検証（ADR 0015 第1層。CI は mock のみ、実 API なし）
- canonical message 変換の round-trip test（OpenAI ↔ canonical ↔ Anthropic）
- usage イベントの event stream への emit テスト
- `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Related Links

- intents/evorch/features/provider-routing/overview.md
- intents/evorch/technology/re-evaluation-2026-08.md
- intents/evorch/decisions/0004-provider-routing-separation.md
- intents/evorch/decisions/0015-verification-two-layer.md
- intents/evorch/decisions/0012-metrics-architecture.md
- intents/evorch/technology/mvp-roadmap.md
- Predecessor: v01-scaffold / v01-event-stream

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `features/provider-routing` overview（v0.1 3種確定を mvp-roadmap に記録）
- ADR candidate: ADR 0016（統一 canonical message 正規化と OpenAI / Anthropic / OpenAI-compatible 変換）
- Diagram candidate: none
- Docs update: none（role-facing surface を追加しないため）
- Closeout writeback expected: yes（ADR 0016 + provider-routing overview + mvp-roadmap）

## Guide Reachability (G645)

While the author still knows the answer, name the guide surface and role that route to every
role-facing surface this slice adds, or explicitly say that no role-facing surface is added. A
blank answer is not treated as no-surface. The closeout record is a debt check, not a merge gate.

この slice は role-facing surface（CLI / GUI / 対話 surface）を追加しない。内部 crate（crates/providers/）のみの変更であり、`no_role_facing_surface: true` を宣言する。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.