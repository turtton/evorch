# v0.4: codex サブスクリプションのログイン導線（GUI からの認証開始/状態表示）

## Goal

operator が GUI から codex サブスクリプション（OpenAI Codex / ChatGPT subscription）の認証を開始でき、認証状態を確認できる導線を提供する。既存の device OAuth / トークン管理を provider 層から再利用し、平文 credential 禁止・fail-closed 規約を維持する。

## Why This Slice Exists Now

v02-provider-codex-subscription で provider 層の認証・トークン管理は実装されたが、GUI からどこからログインするのかが分からないという operator フィードバックがある。v0.4 で実用化のための導線を追加する必要がある。

## Current Observed State

- `crates/providers/src/provider/codex/oauth/device.rs`: `DeviceAuthClient` による device authorization フロー（user_code / verification url / polling）が実装済み
- `crates/providers/src/provider/codex/tokens.rs`: `CodexTokenStore`、トークン bundle の JSON 永続化を実装済み
- `crates/providers/src/provider/codex/session.rs`: `CodexSessionManager` が credential store 統合
- `crates/providers/src/provider/codex/client.rs`: `CodexClient` が `ProviderClient` 実装
- `crates/gui/src/`: codex 認証開始・状態表示の GUI 導線は存在しない
- provider settings（v03-gui-provider-settings）は openai-compatible プロバイダのみ対象

## Accepted Baseline You May Assume

- Rust 1.97 / edition 2024 + egui（workspace Cargo.toml）
- v0.3 確定: Provider Settings surface（OpenAI 互換）、GUI workbench restructure（PR #66）
- ADR 0008 credential 分離（平文 credential 禁止・keychain 優先）
- 認証トークンの実体は provider 層 / credential store に閉じ、GUI は状態表示のみを扱うこと

## Target Repo / Path / Part

- Repo: turtton/evorch
- Target paths: crates/gui/ crates/providers/
- Part: GUI からの codex 認証開始・状態表示 surface、既存 device OAuth / token store との接続

## In Scope

- Settings/providers surface への codex 認証導線追加
- 認証状態の GUI 表示（未認証 / 認証中・user_code 表示 / 認証済み）
- `DeviceAuthClient` + `CodexTokenStore` の GUI 利用（重複実装を避ける）
- 未認証時の案内

## Out Of Scope

- 新規 OAuth フローの実装（既存を流用）
- トークン永続化方式の変更（既存 store を流用）
- 平文 API key / token の GUI 入力（禁止）

## Standalone Child Issue Contract

本 PR は GUI から codex 認証を開始し状態を確認できることを単独で示す。実際の codex API 呼び出しは provider 層の既存実装を利用する。

## Acceptance Criteria

packet の acceptance_criteria が権威（5 件: 認証開始 / 状態表示 / 未認証案内 / credential 規約維持 / headless capture 検証と品質ゲート）。

## Verification

- focused tests: GUI 認証開始フロー test、状態遷移 test、トークン実体非接触 test
- headless capture で認証導線と状態遷移を検証
- `cargo test --workspace` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --check` / `git diff --check` 全 pass
- Reviewer Gate（3+ files のため必須）blocker 0 / 承認済み

## Related Links

- intents/evorch/features/gui-workbench/overview.md
- intents/evorch/features/provider-routing/overview.md
- intents/evorch/decisions/0008-threat-model-phased-adoption.md
- 関連: v02-provider-codex-subscription（provider 層実装）、v03-gui-provider-settings（Settings surface）

## Knowledge Maintenance

- Intent placement: gui-workbench overview へ codex 認証導線の確定を反映（lead が closeout 時に実施）
- ADR candidate: none
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: yes（gui-workbench overview）

## Guide Reachability (G645)

- guide_surface: gui-workbench overview の codex ログイン導線
- role: Operator
- target_surface: Settings surface（OpenAI Codex 認証開始・状態表示）

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
