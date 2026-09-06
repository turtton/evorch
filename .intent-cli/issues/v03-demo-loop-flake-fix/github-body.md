# v0.3: demo_loop テスト flake の根治（goal 結合 race / 非決定シーケンスの決定論化）

## Goal

`crates/gui/tests/demo_loop.rs` の `demo_goal_reaches_awaiting_merge_then_complete_deterministically` など demo loop テストの非決定的失敗（flake）を根治する。waiting→completion 遷移の timing 依存（poll interval / event ordering / goal 結合 race）を特定し、sleep 依存の撤廃と event 待機ベースの決定論化で固定化する。

本 PR は Closes #77 となる。

## Why This Slice Exists Now

PR #74（orchestrator-loop）で導入された demo loop テストが CI で非決定的に失敗し、PR #76 / #82 の変更とは無関係に同一 head SHA で旧 run fail / 最新 run pass を繰り返している（CI runs 89065321929 fail → 89065899906 pass、直近 PR #82 CI push run で再発して rerun で pass）。CI の信頼性を毎 run 毎に削っており、以降の v0.3 slice 委譲を前提に根治が必要。

## Current Observed State

- `crates/gui/tests/demo_loop.rs`: `demo_goal_reaches_awaiting_merge_then_complete_deterministically` が同一 head SHA で fail / pass を繰り返す
- テストは `assert_milestone_order`（milestone イベントの出現順序）と `normalized_sequence`（ID / タイミング除去後のイベント列一致）で決定性を検証
- runtime 側の goal supervisor / orchestrator loop が waiting→awaiting_merge→complete 遷移を非同期的に進める構造で、demo fixture 駆動側に固定待機が残っている可能性がある

## Accepted Baseline You May Assume

- Rust 1.97 / edition 2024 + Tokio async runtime（workspace Cargo.toml）
- v02 の確定: orchestrator loop（PR #74）、direct escalation handoff（PR #76）、GUI workbench restructure（PR #66）
- demo loop テストの fixture / milestone 検出基盤は現行を流用する

## Target Repo / Path / Part

- Repo: turtton/evorch
- Target paths: crates/gui/ crates/runtime/
- Part: demo_loop テスト flake の根治（waiting→completion 遷移の timing 依存の特定と決定論化、sleep 依存の撤廃、event 待機ベース化）

## In Scope

- 高負荷条件（taskset -c 0,1 等）での flake 再現と根本原因の特定
- 根本原因の決定論化（sleep / poll interval 依存の撤廃、event 待機ベースの milestone 検出等）
- demo fixture 駆動の待機パターンの修正
- 高負荷下での連続 pass 検証

## Out Of Scope

- demo loop シナリオ自体の変更（demo の振る舞い・イベント列の意味論は維持）
- orchestrator loop / goal supervisor の機能追加
- 他テストの非決定性対応（本 slice は demo_loop の flake に限定）

## Standalone Child Issue Contract

本 PR は demo_loop テストが高負荷条件でも決定的に pass することを単独で示す。CI 信頼性の回復がゴールであり、他 slice への依存はない。

## Acceptance Criteria

packet の acceptance_criteria が権威（5 件: 高負荷再現と根本原因特定 / 決定論化 / 負荷下連続 N 回 pass / 既存テスト回帰なし / 品質ゲート）。

## Verification

- 高負荷再現: `taskset -c 0,1 cargo test -p <pkg> demo_` 等の連続実行で原因特定の記録
- 修正後の連続 pass 記録（N≥20 回、コマンドと結果）
- `cargo test --workspace` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --check` / `git diff --check` 全 pass
- Reviewer Gate（3+ files のため必須）blocker 0 / 承認済み

## Related Links

- intents/evorch/features/gui-workbench/overview.md
- intents/evorch/features/agent-runtime-kernel/overview.md
- 関連 issue: #77（本 PR が Closes #77）
- 発生履歴: PR #76 CI runs 89065321929 / 89065899906、PR #82 CI push run

## Knowledge Maintenance

- Intent placement: gui-workbench overview へ flake 根本原因と決定論化の確定方式を反映（lead が closeout 時に実施）
- ADR candidate: none
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: yes（gui-workbench overview、必要なら agent-runtime-kernel overview）

## Guide Reachability (G645)

- no_role_facing_surface: true（テストのみの修正でユーザー対向面は不変）

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
