# v11-b: runtime の独自 RecordingMockOpenAi を crates/mock-openai へ統合（テスト fixture 重複解消）

## Goal

crates/runtime/tests/support/mock_openai.rs の独自 HTTP fixture `RecordingMockOpenAi` を廃止し、唯一の利用箇所 `provider_correlation.rs` を crates/mock-openai の `StreamingMockOpenAi` + `recorded_requests()` へ移行して、OpenAI 互換テスト fixture を単一実装に統一する。

## Why This Slice Exists Now

テスト資産の棚卸しで、runtime の tests/support に crate 版と重複する独自 fixture が残存していることが判明した。独自版は crate 版の劣化 subset（記録項目が少ない・SSE 非対応・枯渇時ハング・shutdown なし）であり、今後 fixture 拡張（v11-a）を crate 側に入れても独自版には波及しない。二重管理を解消して拡張の恩恵を全テストへ均質化する。

## Current Observed State

- `crates/runtime/tests/support/mock_openai.rs`: 約100行の独自実装。`RecordedRequest` は `authorization` / `body` のみ。script は生 JSON 文字列（`Vec<String>`）で SSE 非対応、枯渇時は `pop_front` 後 `continue` で無応答ハングの危険、accept thread の shutdown 機構なし
- 利用は `provider_correlation.rs` のみ（`spawn(vec![tool_response, stop_response])` + request 観測 + run_id 相関 assertion）
- crate 側 `StreamingMockOpenAi` は `RecordedRequest { method, path, authorization, body, stream }` / `recorded_requests()` / Drop による listener shutdown を備える superset
- 相違は応答 body の自由度のみ（生文字列 script vs ScriptedResponse 生成物）

## Accepted Baseline You May Assume

- crates/mock-openai は後方互換を維持した additive 拡張のみ許容（v11-a と同ポリシー）
- provider_correlation.rs の検証意図（event-bus 観測と wire request の run_id 一致）は不変

## Target Repo / Path / Part

- Repo: turtton/evorch
- Target paths: crates/runtime/tests/ crates/mock-openai/
- Part: RecordingMockOpenAi の廃止と crate fixture への移行（必要時のみ crate 側 additive 拡張）

## In Scope

- provider_correlation.rs の crate fixture 移行（同等 assertion の維持）
- support/mock_openai.rs 削除と mod 宣言除去
- 必要と判断した場合のみ crates/mock-openai への additive API 追加（例: raw body script）とテスト

## Out Of Scope

- crates/mock-openai の error scripting / matched dispatch（v11-a のスコープ）
- 他の tests/support ユーティリティ（terminal_fence 等）の整理
- providers/tests の wiremock contract の見直し

## Standalone Child Issue Contract

本 PR は fixture 重複の解消を単独で示す。v11-a への hard dependency はないが、raw body script API の要否判断に v11-a の結果が材料になるため、実施順は v11-a → v11-b を推奨。

## Acceptance Criteria

packet の acceptance_criteria が権威（5 件: 独自 fixture 削除 / 同等 assertion 維持 / crate 拡張要否の判断記録 / runtime テスト pass / 品質ゲート）。

## Verification

- focused tests: `cargo test -p runtime`（provider_correlation を含む）、crate 側を拡張した場合は `cargo test -p mock-openai`
- `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --check` / `git diff --check` 全 pass
- Reviewer Gate（test support の構造変更のため推奨）blocker 0 / 承認済み

## Related Links

- intents/evorch/features/agent-runtime-kernel/overview.md
- 関連: v03-mock-streaming-provider-e2e（crate fixture 新設）、v11-a-mock-openai-error-scripting（crate 側拡張）

## Knowledge Maintenance

- Intent placement: 反映不要（内部整理）。closeout メモで fixture 局所実装 vs crate 化の基準を残す
- ADR candidate: none
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: no

## Guide Reachability (G645)

- 内部テスト基盤の整理のため role-facing の案内面は新設しない

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
