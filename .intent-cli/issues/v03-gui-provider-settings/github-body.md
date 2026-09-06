# v0.3: GUI provider 設定 surface（base_url / api_key_env / model 選択）

## Goal

provider 設定の導線を GUI から提供する（現状は evorch.toml 直編集のみ）。Settings surface（新規 panel またはモーダル）から openai-compatible provider の設定（base_url / api_key_env / model）と default_model 選択ができるようにする。credential は api_key_env の環境変数名のみ（平文禁止、fail-closed 規約維持）。

## Why This Slice Exists Now

v03-provider-composition-root で provider 設定 schema（evorch.toml [providers.*]）は確定したが、導線がファイル直編集のみで operator には使いにくい。v03-gui-chat-composer の未設定ガード誘導先としても必要であり、v0.3 の実用化のために Settings surface が求められる。

## Current Observed State

- crates/config/: [providers.*] openai-compatible variant（base_url / api_key_env / models / default_model）は確定済み（deny_unknown_fields 維持）
- crates/gui/: Settings surface は存在せず、provider 設定は evorch.toml 直編集のみ
- credential は api_key_env の環境変数名のみ（平文混入は fail-closed 拒否、ADR 0008）

## Accepted Baseline You May Assume

- Rust 1.97 / edition 2024 + egui（workspace Cargo.toml）
- v03 の確定: provider composition root + openai-compatible 設定 schema（evorch.toml）
- ADR 0008 credential 分離（keychain 優先・0600 fallback）の脅威モデル

## Target Repo / Path / Part

- Repo: turtton/evorch
- Target paths: crates/gui/ crates/config/
- Part: Settings surface（provider 設定の表示/編集、default_model 選択、evorch.toml 書き戻し、未設定誘導）

## In Scope

- Settings surface の新設（新規 panel またはモーダル）
- openai-compatible provider の base_url / api_key_env / models / default_model の表示・編集
- evorch.toml への保存（schema v2 維持、additive のみ、migration 不要）
- credential 入力ガード（api_key_env のみ、平文拒否）
- provider 未設定時の composer / goal 実行前誘導

## Out Of Scope

- keyring への API key 保存 UI（既存 codex パターンへの接続余地として扱う）
- OpenAI 互換以外の provider 設定 UI
- 設定の暗号化・トークン管理画面

## Standalone Child Issue Contract

本 PR は GUI から provider 設定を完結できることを単独で示す。credential の keyring 保存は後続 slice。

## Acceptance Criteria

packet の acceptance_criteria が権威（5 件: 設定表示/編集 / evorch.toml 保存（schema 維持）/ credential ガード / 未設定誘導 / headless capture 検証と品質ゲート）。

## Verification

- focused tests: Settings surface の表示・編集・保存 test、平文 credential 拒否 test、未設定誘導 test
- headless capture で設定 surface を検証
- `cargo test --workspace` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --check` / `git diff --check` 全 pass
- Reviewer Gate（3+ files のため必須）blocker 0 / 承認済み

## Related Links

- intents/evorch/features/gui-workbench/overview.md
- intents/evorch/features/provider-routing/overview.md
- intents/evorch/decisions/0008-threat-model-phased-adoption.md
- 関連: v03-provider-composition-root（設定 schema 確定）、v03-gui-chat-composer（未設定誘導の発火元）

## Knowledge Maintenance

- Intent placement: gui-workbench overview へ Settings surface の確定構成を反映（lead が closeout 時に実施）
- ADR candidate: none
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: yes（gui-workbench overview）

## Guide Reachability (G645)

- guide_surface: gui-workbench overview の provider 設定 surface
- role: Operator
- target_surface: Settings surface（base_url / api_key_env / model 選択）

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
