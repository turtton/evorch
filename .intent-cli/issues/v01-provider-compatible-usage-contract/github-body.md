## Goal

OpenAI-compatible endpoint の UsageEvent が streaming / non-streaming と retry/fallback の completed request で exactly once になる contract test と文書保証を追加する。

## Why This Slice Exists Now

v0.1 inspect の slice #4 で、usage 実装はあるが direct exactly-once contract が不足する低 severity gap が確認された。OpenAI-compatible non-stream test は usage label/model を1件読むものの二件目不在を確認せず、stream test は bus を接続しない（`crates/providers/tests/openai_compatible_contract.rs:34-102`）。shared SSE unit test は一回性を持つ（`crates/providers/src/http/stream/tests.rs:124-176`）が、compatible client と retry/fallback across attempts の保証にはなっていない。

## Current Observed State

- OpenAI-compatible は `ChatCompletionsClient` を共有する（`crates/providers/src/provider/openai_compatible.rs:19-78`; `crates/providers/src/provider/openai.rs:100-133`）。
- non-stream send は response 変換後に usage を emit する（`crates/providers/src/provider/openai.rs:136-171`）。
- stream は completion signal で accumulator を確定し usage を emit して `Completed` を出す（`crates/providers/src/http/stream.rs:156-171`）。
- `UsageEmitter` は caller が1 requestにつき1回呼ぶ契約だが、発行器自身に dedupe state はない（`crates/providers/src/http.rs:114-150`）。
- `Router::next_fallback` は候補選択と affinity 更新のみで request execution / accounting を行わない（`crates/routing/src/router.rs:150-218`）。

## Accepted Baseline You May Assume

- Rust edition 2024 / rust-version 1.97、Tokio 1、serde 1、reqwest 0.12、wiremock 0.6。
- UsageEvent は provider / model / input/output/cache token fields を持つ。
- OpenAI / Anthropic / OpenAI-compatible の mock HTTP/SSE contract tests は実装済み。
- shared SSE pump unit test は completion 時の usage 一回性を検証済み。

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/providers/`, `crates/routing/`

Target part: OpenAI-compatible usage accounting contract tests、retry/fallback attempt accounting。

## In Scope

- compatible non-stream / stream completed request の exactly-one UsageEvent。
- error / timeout / invalid response / no completion / early drop の zero UsageEvent。
- retry/fallback で失敗attempt 0件、勝者completed attempt 1件、logical request合計1件。
- 勝者 provider profile label / model / token payload の検証。
- provider completion pathが唯一のusage発行所有者でcoordinatorは再発行しない契約の文書化。

## Out Of Scope

- routing order、retry policy、cooldown / affinity の変更。
- 新 provider、UsageEvent schema、aggregator/storage の変更。
- provider observation / TTFT schema（別 execution unit）。
- 実 API test。

## Standalone Child Issue Contract

OpenAI-compatible client の mock HTTP/SSE contract testsを拡張し、non-stream / stream の各 completed requestで正しい UsageEventがちょうど1件、error・incomplete・early-drop attemptでは0件であることを証明してください。retry/fallback harnessでは敗者attempt 0件、最終勝者attempt 1件、logical request合計1件とし、勝者profile label/modelだけが記録されることを確認します。usageの唯一の発行所有者はprovider attempt completion pathで、routing/coordinatorは再発行しないと文書化してください。routing policy、provider追加、usage schema変更は行いません。

## Acceptance Criteria

- [ ] compatible non-stream completed request が正しい payload の UsageEvent をちょうど1件発行する。
- [ ] compatible stream Completed が複数delta/usage/DONE/tail条件でも UsageEvent をちょうど1件発行する。
- [ ] HTTP/transport/parse error、completion無し、Completed前dropは UsageEvent 0件である。
- [ ] retry/fallback は失敗attempt 0件、最終completed attempt 1件、logical request合計 exactly once である。
- [ ] fallback勝者の provider label / modelだけがusageに記録され、敗者・coordinatorから重複発行されない。
- [ ] usage発行所有権と completed attempt の定義が rustdoc/docs に明記される。
- [ ] OpenAI / Anthropic の既存usage testsを壊さず、全検証がwiremockで完結する。
- [ ] routing/retry policy、新provider、usage schema/aggregationを変更しない。

## Verification

- `cargo test -p providers --test openai_compatible_contract`
- `cargo test -p providers`
- `cargo test -p routing`
- `cargo check --workspace`
- `git diff --check`

## Related Links

- `intents/evorch/features/provider-routing/overview.md:5-21,23-30,41-45`
- `intents/evorch/decisions/0004-provider-routing-separation.md:11-36`
- v0.1 slice #4 / issue #4 (`v01-provider-client`)

## Knowledge Maintenance

- Intent placement: provider-routing（primary）、ADR 0004（supporting）。新規 node なし。
- ADR candidate: none。
- Diagram candidate: none。
- Docs update: provider-routing overview に completed attempt / coordinator の usage発行所有権と exactly-once保証を追記。
- Closeout writeback expected: yes。

## Guide Reachability (G645)

`no_role_facing_surface: true`。内部 contract tests と accounting guarantee だけを追加し、新しい role-facing surface はない。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
