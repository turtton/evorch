# v0.3: OpenAI 互換 streaming mock server + 高レート streaming 安定性検証

## Goal

実 provider の代わりに OpenAI API 形式（chat completions streaming SSE）で応答を返すモック server を test/fixture として新設し、通常応答・tool call 応答を返せるようにする。さらに 1500 tok/s 程度の高レート streaming をシステム全体（event-bus → GUI transcript）が落ちずに処理できることを検証するテストを作成する。

## Why This Slice Exists Now

v03-provider-composition-root で headless 実 run の経路が成立したが、実 provider 依存の E2E は network mode-lock に依存しており、streaming の高負荷挙動は未検証。v0.3 で GUI chat composer（v03-gui-chat-composer）や実運用を見据え、streaming 経路の安定性基盤を先行して作る必要がある。

## Current Observed State

- OpenAiCompatibleClient（crates/providers/src/http/openai_compat.rs）は実装済み（SSE streaming 対応）
- headless 実 run の構成経路（compose_runtime）は存在するが、provider 応答は実 API 依存または fixture 依存
- event-bus → GUI transcript 経路のスループット・詰まり特性は未計測

## Accepted Baseline You May Assume

- Rust 1.97 / edition 2024 + Tokio async runtime（workspace Cargo.toml）
- v03 の確定: provider composition root（headless 実 run 構成）
- mock server の配置（crates/providers/tests/support/ または新規 crate）は実装時に調査して決定してよい

## Target Repo / Path / Part

- Repo: turtton/evorch
- Target paths: crates/providers/ crates/runtime/ crates/gui/
- Part: OpenAI 互換 SSE streaming mock server（test/fixture）+ tool call 応答対応 + 1500 tok/s burst 安定性検証テスト

## In Scope

- OpenAI 互換 SSE streaming mock server の新設（test/fixture、通常応答 + tool call 応答）
- compose_runtime 経由の headless run で mock server に接続する E2E
- 1500 tok/s 相当 burst streaming の drop/詰まり検証テスト（計測記録付き）
- 決定論的駆動（tick/flush ベース、CI 安定）

## Out Of Scope

- 実 provider への load test
- streaming プロトコルの OpenAI 非互換拡張
- GUI の streaming 表示最適化（本 slice は基盤検証まで）

## Standalone Child Issue Contract

本 PR は mock server による headless run E2E と高レート安定性検証を単独で示す。composer 等の利用側 slice とは独立。

## Acceptance Criteria

packet の acceptance_criteria が権威（5 件: mock server 応答対応 / headless E2E / 1500 tok/s burst 検証 / CI 決定論性 / 品質ゲート）。

## Verification

- focused tests: mock server の通常/tool call 応答 test、headless E2E、burst 検証（計測記録）
- `cargo test --workspace` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --check` / `git diff --check` 全 pass
- Reviewer Gate（3+ files のため必須）blocker 0 / 承認済み

## Related Links

- intents/evorch/features/agent-runtime-kernel/overview.md
- intents/evorch/features/provider-routing/overview.md
- 関連: v03-provider-composition-root（headless 構成基盤）

## Knowledge Maintenance

- Intent placement: agent-runtime-kernel overview へ mock server 構成と burst 検証結果を反映（lead が closeout 時に実施）
- ADR candidate: none
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: yes（agent-runtime-kernel overview）

## Guide Reachability (G645)

- no_role_facing_surface: true（test/fixture 基盤でユーザー対向面は不変）

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
