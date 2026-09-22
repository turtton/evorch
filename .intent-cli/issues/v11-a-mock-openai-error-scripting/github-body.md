# v11-a: mock-openai の error response scripting + request 条件付き dispatch（テスト fixture 拡張）

## Goal

crates/mock-openai の OpenAI 互換 fixture を拡張し、(a) 任意 HTTP status（401 / 429 / 500 等）と OpenAI 形式 error body を script できる error 応答 API と、(b) request 内容（method / path / body の model 等）に基づいて script を選択する opt-in の matched dispatch を追加する。

## Why This Slice Exists Now

テスト資産の棚卸しで、E2E 系テスト（runtime / gui / arena / evorch）は mock-openai に統一されている一方、エラー系検証は providers/tests の wiremock contract test にしか書けず、E2E 文脈（agent loop 中の provider 失敗、stream 中のエラー、不正設定時の admission 失敗）を mock-openai 系テストでは表現できないことが判明した。fixture 側の最小拡張でこの不整合を解消し、異常系 E2E の書き方を統一する。

## Current Observed State

- `crates/mock-openai/src/scenario.rs`: `ScriptedResponse` は `text_stream` / `tool_call` のみ。`finish_reason` は `stop` / `tool_calls` 固定で、error 応答・任意 status を構成不能
- `crates/mock-openai/src/server.rs`: `GET /v1/models` 以外は method / path 不問で script queue を `pop_front` する純粋 FIFO。枯渇時は固定 500 `{"error":{"message":"unscripted request","type":"mock_openai"}}`。`write_sse` / `write_json` は 200 OK 固定
- `WriteMode` は `FramePerWrite` / `WholeBody` / `GatedAfterFirst`。SSE 途中の error / 切断注入は不可
- `RecordedRequest { method, path, authorization, body, stream }` と `recorded_requests()` は実装済みで、条件判定材料は揃っている
- エラー系は `providers/tests/*.rs` が wiremock で検証中（429 + Retry-After / 500 / malformed SSE 等）

## Accepted Baseline You May Assume

- v03-mock-streaming-provider-e2e 確定: crates/mock-openai の SSE fixture（ScriptedResponse / StreamingMockOpenAi / WriteMode）
- v04-provider-settings-ux 確定: `/v1/models` endpoint と `spawn_with_models`
- provider 層のエラー contract は wiremock 側に既存。本 slice は fixture 拡張であり既存 wiremock test の移行は行わない

## Target Repo / Path / Part

- Repo: turtton/evorch
- Target paths: crates/mock-openai/
- Part: error response scripting API + opt-in matched dispatch

## In Scope

- ScriptedResponse への error 応答 API 追加（任意 status + error body、additive）
- streaming / 非 streaming 双方に対する error script の適切な応答形（stream 判定の尊重）
- opt-in matched dispatch と fallback 規則
- crates/mock-openai/tests/ への検証テスト追加（実 client 経路を含む）

## Out Of Scope

- SSE mid-stream での切断 / error 注入用の WriteMode 拡張（必要なら別 slice）
- providers/tests の wiremock contract test の mock-openai 移行
- /v1/responses 等他 endpoint の追加、request schema 検証、認証検証
- crates/runtime/tests/support/mock_openai.rs（RecordingMockOpenAi）の統合 → v11-b-recording-mock-consolidation

## Standalone Child Issue Contract

本 PR は mock-openai fixture の additive 拡張を単独で示す。既存 consumer の無変更互換を acceptance に含む。関連 slice v11-b（RecordingMockOpenAi 統合）とは独立に実施可能だが、raw body script の要否判断を v11-b で行う関係上、推奨順序は本 slice が先。

## Acceptance Criteria

packet の acceptance_criteria が権威（6 件: error script API / streaming 判定の尊重 / matched dispatch + fallback 規則 / self-test 追加 / 既存 consumer 互換 / 品質ゲート）。

## Verification

- focused tests: crates/mock-openai/tests/ の新規 error / matched dispatch テスト、client_stream_contract 系の既存テスト
- `cargo test -p mock-openai` および workspace 全体の既存 mock 利用テストが無変更で pass
- `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --check` / `git diff --check` 全 pass
- Reviewer Gate（既存 API 境界の追加のため推奨）blocker 0 / 承認済み

## Related Links

- intents/evorch/features/agent-runtime-kernel/overview.md
- intents/evorch/features/provider-routing/overview.md
- 関連: v03-mock-streaming-provider-e2e（fixture 新設）、v04-provider-settings-ux（/v1/models 追加）、v11-b-recording-mock-consolidation

## Knowledge Maintenance

- Intent placement: agent-runtime-kernel overview へ確定 API と fallback 規則を反映（lead が closeout 時に実施）
- ADR candidate: none
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: yes（agent-runtime-kernel overview）

## Guide Reachability (G645)

- 内部テスト基盤の拡張のため role-facing の案内面は新設しない

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
