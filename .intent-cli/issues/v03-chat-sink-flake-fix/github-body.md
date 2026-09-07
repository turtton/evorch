# v0.3: chat_sink_runtime flake 根治（second_chat_reuses_same_run_and_run_survives_resume 非決定失敗）

## Goal

`crates/gui/tests/chat_sink_runtime.rs:140` の `second_chat_reuses_same_run_and_run_survives_resume` の非決定的失敗を根治する。PR #84 の demo_loop flake fix 同様、sleep/poll 依存を撤廃し、event 待機ベースの決定論化で固定化する。

本 PR は Closes #95 となる。

## Why This Slice Exists Now

PR #92 で導入された chat sink テストが CI で複数回非決定的に失敗している（PR #94 push run、main revert run）。rerun で pass するため開発効率と CI 信頼性を損なっており、v0.3 以降の委譲前に根治したい。

## Current Observed State

- `crates/gui/tests/chat_sink_runtime.rs:171` の `second_chat_reuses_same_run_and_run_survives_resume` が fail / rerun pass を繰り返す
- `wait_for_reply` は `tokio::time::timeout(Duration::from_secs(5), ...)` で `MessageDelta { run_id: Some(id) }` と `AgentRunStateChanged { to: Waiting }` を待機している
- `crates/gui/src/runtime_sink.rs:170-197` では `chat_runs` キャッシュを持ち、2 回目の chat は `runtime.send_message(run_id, ...)` を試行。失敗すると新規 run を起動する。同一 run_id 維持には 1 回目の run が Waiting 状態で生存している必要がある
- 高負荷時に timeout または event 順序で fail すると推定される

## Accepted Baseline You May Assume

- Rust 1.97 / edition 2024 + Tokio async runtime（workspace Cargo.toml）
- v0.3 確定: chat composer / chat sink（PR #92）、event run_id attribution（v03-event-runid-attribution）
- PR #84 の demo_loop flake fix 事例を先例とする

## Target Repo / Path / Part

- Repo: turtton/evorch
- Target paths: crates/gui/ crates/runtime/
- Part: chat_sink_runtime テストの非決定失敗の根治（event 待機ベース決定論化、timeout 見直し、run 状態復帰の保証）

## In Scope

- 高負荷再現と根本原因の特定
- sleep/poll 依存の撤廃、event 待機ベースの milestone 検出
- `taskset -c 0,1` 負荷下での N≥30 連続 pass 検証

## Out Of Scope

- chat sink の機能変更（再利用・run 起動の意味論は維持）
- chat composer UI の変更
- 他テストの flake 対応

## Standalone Child Issue Contract

本 PR は `second_chat_reuses_same_run_and_run_survives_resume` が高負荷でも決定的に pass することを単独で示す。他 slice への依存はない。

## Acceptance Criteria

packet の acceptance_criteria が権威（5 件: 根本原因特定 / 決定論化 / N≥30 連続 pass / 回帰なし / 品質ゲート）。

## Verification

- 高負荷再現: `taskset -c 0,1 cargo test -p evorch-gui second_chat_reuses_same_run_and_run_survives_resume` 等の連続実行で原因特定の記録
- 修正後の連続 pass 記録（N≥30 回、コマンドと結果）
- `cargo test --workspace` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --check` / `git diff --check` 全 pass
- Reviewer Gate（3+ files のため必須）blocker 0 / 承認済み

## Related Links

- intents/evorch/features/gui-workbench/overview.md
- intents/evorch/features/agent-runtime-kernel/overview.md
- 関連 issue: #95（本 PR が Closes #95）
- 発生履歴: PR #94 push run、main revert run、PR #92（chat sink 導入）
- 先例: PR #84（demo_loop flake fix）

## Knowledge Maintenance

- Intent placement: gui-workbench overview へ flake 根本原因と決定論化の確定方式を反映（lead が closeout 時に実施）
- ADR candidate: none
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: yes（gui-workbench overview）

## Guide Reachability (G645)

- no_role_facing_surface: true（テスト修正でユーザー対向面は不変）

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
