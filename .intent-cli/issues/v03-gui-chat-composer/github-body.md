# v0.3: GUI chat composer（senpi 踏襲の通常チャット + スラッシュコマンド）

## Goal

GUI workbench に chat composer を新設する。senpi がやっているように、通常メッセージはチャットとして agent に送り、/goal 等のスラッシュコマンドで goal 設定等のコマンドを発行できる composer にする。現状 GUI は goal 設定しかできず、通常の agent harness としてのチャットができない状態を解消する。

## Why This Slice Exists Now

v02 までで agent harness としての runtime メッセージング（send / reply / steering）は成立したが、GUI からの入力経路は goal 設定のみ。operator が日常的に使うには通常チャットが必須であり、v0.3 の workbench 完成の前段として composer が必要。

## Current Observed State

- crates/gui/src/: workbench は goal 設定経路を持つが、通常チャット用 composer は存在しない
- crates/runtime/: AgentMessage の send / reply / steering と transcript 永続化は v02 で実装済み
- provider 未設定時の挙動ガードは未実装

## Accepted Baseline You May Assume

- Rust 1.97 / edition 2024 + egui（workspace Cargo.toml）
- v02 の確定: agent messaging、goal 設定フロー、GUI workbench restructure（PR #66）
- 実装上 v03-mock-streaming-provider-e2e の mock があると検証しやすいが、依存はなく従来 fixture でも検証可能

## Target Repo / Path / Part

- Repo: turtton/evorch
- Target paths: crates/gui/ crates/runtime/
- Part: chat composer（通常チャット送信 + /goal 等のスラッシュコマンド + コマンド一覧 + provider 未設定ガード）

## In Scope

- composer UI の新設（通常メッセージ送信）
- スラッシュコマンド処理（/goal で既存 goal 設定フローへ接続、/help でコマンド一覧）
- provider 未設定時のエラー/案内表示
- headless/kittest ベースの操作テスト

## Out Of Scope

- composer 以外の入力 surface（terminal pane 等の拡張）
- プロバイダ設定 UI 自体（v03-gui-provider-settings が担当）
- multi-line リッチ編集・添付等の高度な入力機能

## Standalone Child Issue Contract

本 PR は GUI から通常チャットと /goal コマンドの両方が使えることを単独で示す。provider 設定 UI は後続 slice。

## Acceptance Criteria

packet の acceptance_criteria が権威（5 件: 通常チャット / /goal 互換 / コマンド一覧 / 未設定ガード / テストと品質ゲート）。

## Verification

- focused tests: composer 入力→メッセージ送信 test、/goal コマンド test、provider 未設定ガード test
- headless または kittest ベースの操作テスト
- `cargo test --workspace` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --check` / `git diff --check` 全 pass
- Reviewer Gate（3+ files のため必須）blocker 0 / 承認済み

## Related Links

- intents/evorch/features/gui-workbench/overview.md
- intents/evorch/features/agent-runtime-kernel/overview.md
- 関連: v03-mock-streaming-provider-e2e（検証の補助として有用）、v03-gui-provider-settings（未設定ガードの誘導先）

## Knowledge Maintenance

- Intent placement: gui-workbench overview へ composer surface とコマンド方式を反映（lead が closeout 時に実施）
- ADR candidate: none
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: yes（gui-workbench overview）

## Guide Reachability (G645)

- guide_surface: gui-workbench overview の chat composer
- role: Operator
- target_surface: workbench のチャット入力欄とスラッシュコマンド

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
