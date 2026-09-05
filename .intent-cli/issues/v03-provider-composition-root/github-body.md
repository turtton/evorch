# v0.3: provider composition root + OpenAI 互換 provider 設定で headless 実 run E2E

## Goal

config（evorch.toml）に記述した OpenAI API 互換 provider（base_url / API key / モデル）から provider client を構築し、AgentRuntime へ注入する composition root を新設する。GUI 非依存の headless 経路で実 provider への実 run E2E まで成立させる。

## Why This Slice Exists Now

v02 完了時点で agent loop / orchestration / tools は揃ったが、実 provider に接続する配線（config → factory → runtime 注入）が存在しない。実運用の動作確認（エージェントが実 provider 応答を生成するか）に必要な最小の欠落を埋める。

## Current Observed State

- OpenAiCompatibleClient は実装済み（crates/providers/src/http/openai_compat.rs）だが構成入口が無い
- config の `[providers.*]` openai-compatible は api_key_env のみの汎用プレースホルダ
- routing factory に型→クライアント構築はあるが runtime/gui から参照する composition root が無い

## Accepted Baseline You May Assume

- Rust 1.97 / edition 2024 + Tokio async runtime（workspace Cargo.toml）
- v02 の確定: prompt assembly（logical model / profile 解決）、routing crate の factory、ADR 0008 credential 分離（keychain 優先）
- crates/providers の client / message / sse / stream 層は流用

## Target Repo / Path / Part

- Repo: turtton/evorch
- Target paths: crates/runtime/ crates/config/ crates/providers/
- Part: provider composition root（config → routing factory → AgentRuntime 注入）+ openai-compatible 設定 schema 確定 + headless 実 run E2E

## In Scope

- provider composition root の新設（GUI/headless/テストの 3 経路で同一構成）
- `[providers.*]` openai-compatible variant の schema 拡張（base_url / api_key_env / models / default_model、deny_unknown_fields 維持）
- API key は環境変数（api_key_env）のみから読む（平文は fail-closed 拒否）
- headless 経路の実 run E2E（deny-by-default network test または recording fixture で mode-lock）

## Out Of Scope

- GUI への実 provider 応答描画（transcript 実配線）
- keyring への API key 保存 UI、credential 管理画面
- OpenAI 互換以外の新規 provider 追加、Responses API 等の別プロトコル

## Standalone Child Issue Contract

本 PR は config 記述の OpenAI 互換 provider で headless 実 run が成立することを単独で示す。GUI 描画・認証 UI は後続 slice。

## Acceptance Criteria

packet の acceptance_criteria が権威（6 件: composition root 3 経路 / config schema 拡張 / env-only credential / headless 実 run E2E / 既定回帰なし / 品質ゲート）。

## Verification

- focused tests: composition root 構築 unit test、credential 平文拒否 test、E2E（mode-lock または deny-by-default network test）
- `cargo test --workspace` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --check` / `git diff --check` 全 pass
- Reviewer Gate（3+ files のため必須）blocker 0 / 承認済み

## Related Links

- intents/evorch/features/agent-runtime-kernel/overview.md
- intents/evorch/decisions/0008-threat-model-phased-adoption.md
- intents/evorch/decisions/0020-canonical-message-normalization.md
- intents/evorch/technology/mvp-roadmap.md
- 関連: #60（codex subscription provider）

## Knowledge Maintenance

- Intent placement: agent-runtime-kernel overview へ composition root 確定構成を反映（lead が closeout 時に実施）
- ADR candidate: none
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: yes（agent-runtime-kernel overview）

## Guide Reachability (G645)

- guide_surface: agent-runtime-kernel overview の provider 配線
- role: Operator
- target_surface: evorch.toml [providers.*] openai-compatible 設定

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
