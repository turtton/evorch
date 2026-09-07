# v0.4: Provider Settings UX 拡張（modal 幅・/v1/models 自動補完・default_model 見直し）

## Goal

Provider Settings modal の UX を拡張する。(a) modal 幅が狭く文字が見切れる問題を viewport 連動（上限 token 付き）で解消 (b) provider の GET /v1/models から model 候補を取得して選択 UI を提供（取得失敗時は手動入力 fallback）(c) 除外したい model 用のフィルタ設定を additive に追加 (d) default_model の存在意義を routing 実態に基づき見直し、維持/廃止を判断する。

## Why This Slice Exists Now

v03-gui-provider-settings で Provider Settings の基本 surface はできたが、operator から (1) modal が狭すぎる (2) model 手動入力が不便 (3) 除外 model 設定が欲しい (4) default_model の存在意義が不明瞭、というフィードバックが出た。v0.4 で実用性を高める。

## Current Observed State

- `crates/gui/src/panes/provider_settings.rs:17` の `provider_settings_modal` は `egui::Modal` を使用。横幅は Modal 既定で狭く、長い URL / env var / model 名が切れる
- `crates/mock-openai/src/server.rs` は `/v1/chat/completions` と汎用 JSON/SSE 応答を返すが `/v1/models` endpoint は未実装
- `crates/config/src/types/provider.rs:144` の `ProviderProfileConfig` は `models: Vec<String>` / `default_model: String` を必須フィールドとして持つ。`deny_unknown_fields` 維持
- `crates/routing/src/router.rs:109-207` の `resolve()` では、session affinity 再解決時および route candidate に `model` 上書きがない場合に `default_model` を concrete model として使用。`routing.routes` が空のときは各論理 model を first profile の `default_model` へ結ぶ。category 別の `logical_model` はあくまで論理 model 名で、concrete model 未指定時は `default_model` が使われる

## Accepted Baseline You May Assume

- Rust 1.97 / edition 2024 + egui（workspace Cargo.toml）
- v0.3 確定: Provider Settings surface、openai-compatible config schema（base_url / api_key_env / models / default_model）、mock-openai fixture
- default_model は routing の fallback anchor として現状は技術的に不可欠だが、UI 上の用途説明が不足ている

## Target Repo / Path / Part

- Repo: turtton/evorch
- Target paths: crates/gui/ crates/config/ crates/providers/
- Part: Provider Settings modal UX（幅_viewport 連動、/v1/models 自動補完、除外フィルタ、default_model 見直し）+ mock-openai /v1/models endpoint

## In Scope

- modal 幅の viewport 連動（上限 theme token 付き）
- GET /v1/models からの model 候補取得と combo box 選択（エラー時は手動入力 fallback）
- 除外モデルフィルタの additive config 追加と GUI 編集
- default_model 見直し（PR body に判断を記録）
- mock-openai の /v1/models endpoint 追加

## Out Of Scope

- keyring への API key 保存 UI
- OpenAI 互換以外の provider 設定 UI
- モデルカタログの手続き的詳細編集

## Standalone Child Issue Contract

本 PR は Provider Settings の UX 拡張 4 点と mock-openai 支援 endpoint を単独で示す。他 slice への依存はない。

## Acceptance Criteria

packet の acceptance_criteria が権威（6 件: modal 幅 / /v1/models 補完 / 除外フィルタ / default_model 見直し / mock endpoint / テストと品質ゲート）。

## Verification

- focused tests: Provider Settings headless capture test（幅・補完・フィルタ保存・default_model 動作）、/v1/models endpoint test
- headless captureで before/after を検証
- `cargo test --workspace` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --check` / `git diff --check` 全 pass
- Reviewer Gate（3+ files のため必須）blocker 0 / 承認済み

## Related Links

- intents/evorch/features/gui-workbench/overview.md
- intents/evorch/features/provider-routing/overview.md
- 関連: v03-gui-provider-settings（基本 surface）、crates/mock-openai

## Knowledge Maintenance

- Intent placement: gui-workbench overview へ Provider Settings UX の確定を反映（lead が closeout 時に実施）
- ADR candidate: none
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: yes（gui-workbench overview）

## Guide Reachability (G645)

- guide_surface: gui-workbench overview の Provider Settings
- role: Operator
- target_surface: Settings surface（provider 追加/編集、model 選択、除外フィルタ）

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
