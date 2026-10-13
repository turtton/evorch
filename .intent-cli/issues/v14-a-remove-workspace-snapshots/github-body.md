# ADR 0031 ①: ワークスペース snapshot・ロック・/undo /redo の廃止

## Goal

書き込み系ツールの呼び出しごとに取っているプロジェクトルート全体のロックと snapshot をやめ、shell のロック待ちをなくす。巻き戻しは git に任せ、`/undo` / `/redo` も削除する。

## Why This Slice Exists Now

shell は書き込むかどうかを事前に判別できないので、`gh pr checks` のような読み取りだけのコマンドもロックを待つ。裏で動き続ける shell ジョブはロックを持ったままになるため、並列の run が互いの `cargo test` や `gh pr checks` を待つ状態が日常的に起きている。ロックの目的は `/undo` / `/redo` で run 単位に変更を戻すことだけで、ロックの単位を細かくしても同じ worktree の中の shell 同士の待ちは残る（ADR 0031）。

## Current Observed State

- `snapshot_before_tool`（`crates/runtime/src/agent_loop/snapshots.rs`）が、`fs_write` 権限を持つツール（`write` / `edit` / `shell`）の前に `SnapshotService::lock_observed` でロックを取り、snapshot を撮って `SnapshotEvent` を発行する。
- shell の場合はガードを `ToolExecutor::retain_shell_call_guard` に預け、ジョブが刈り取られるまでロックを持ち続ける。
- 待ち状況は `LifecycleEvent::WorkspaceWaitChanged` で GUI に通知され、サイドバーに表示される。
- `/undo` / `/redo` は `WorkbenchCommand::RestoreSnapshot` → `AgentRuntime::restore_snapshot` → `SnapshotHistory` で run の変更前に戻す。
- benchmark の記録・再生は `SnapshotStore` の `capture_all` / `restore_all` / `diff` を、`SnapshotService` とは独立に使っている。

## Accepted Baseline You May Assume

- イベントの保存形式は後方互換を保たなくてよい（AGENTS.md の Compatibility policy）。
- 同じ checkout に並列で書き込む run どうしの衝突は runtime では防がない。worktree を分ける運用で防ぐ（ADR 0031）。
- isolated workspace と旧 PR delivery supervisor の削除は v14-b / v14-c で行う。この unit では触らない。

## Target Repo / Path / Part

- Target repo: `turtton/evorch`
- Target paths: `crates/runtime/` `crates/tools/` `crates/event-bus/` `crates/storage/` `crates/gui/` `docs/` `intents/`
- Target part: ツール呼び出し前の snapshot とロック、`/undo` / `/redo`、ワークスペース待ちのイベントと表示。

## In Scope

- `snapshot_before_tool` と、`tool_calls.rs` でのガードの保持・shell への受け渡しの削除。
- `SnapshotService` / `WorkspaceSnapshots` / `WorkspaceSnapshotGuard` / `SnapshotHistory`、`AgentRuntime::with_snapshots` / `restore_snapshot`、`ToolExecutor::retain_shell_call_guard` と `Tool` trait の同名メソッドの削除。
- `SnapshotEvent`、`LifecycleEvent::WorkspaceWaitChanged`、`WorkspaceWait`、`WorkspaceLockHolder` の削除と、storage・otel・GUI の追従。
- GUI の `/undo` / `/redo`、`WorkbenchCommand::RestoreSnapshot`、`LoopEvent::SnapshotRestored`、ワークスペース待ちの表示の削除。`evorch-gui.rs` の `SnapshotService` の組み立ての削除。
- 使われなくなった `SnapshotStore::capture` / `restore` の削除。
- `docs/harness-validation.md` と `docs/harness-runtime-improvements.md` の、snapshot のロック解放に関する記述の削除。

## Out Of Scope

isolated workspace の削除（v14-c）、旧 PR delivery supervisor の削除（v14-b）、benchmark の `SnapshotStore`（`capture_all` / `restore_all` / `diff`）の変更。

## Acceptance Criteria

1. `write` / `edit` / `shell` がワークスペースのロックを取らず、snapshot も撮らない。裏で動く shell ジョブがあっても、同じプロジェクトの別の run の `write` / `edit` / `shell` が待たされない。
2. `SnapshotService`、`WorkspaceSnapshots`、`WorkspaceSnapshotGuard`、`SnapshotHistory`、`with_snapshots` / `restore_snapshot`、`retain_shell_call_guard` が削除されている。
3. `SnapshotEvent`、`WorkspaceWaitChanged`、`WorkspaceWait`、`WorkspaceLockHolder` が削除され、storage・otel・GUI の扱いも削除されている。
4. `/undo` / `/redo` が補完候補と `/help` に出ず、関連する command / event とサイドバーの待ち表示が削除されている。
5. benchmark の記録・再生がこれまでどおり動く。
6. 削除した機能だけを検証していたテストは削除し、ほかの検証も兼ねるテストは snapshot に依存しない形に直す。
7. `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更範囲の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check` が pass する。

## Verification

1. Unit / Integration: 裏で動く shell ジョブを持つ run があるときに、別の run の `write` / `shell` がすぐ実行されること（完了はイベントで待ち、時間待ちに頼らない）。
2. Integration: benchmark の記録・再生のテストが pass すること。
3. Build / lint: 上記のコマンド一式。

## Related Links

- `intents/evorch/decisions/0031-remove-isolated-workspaces.md`
- `intents/evorch/features/tools-sandbox/overview.md`
- `intents/evorch/features/gui-workbench/overview.md`

## Base Branch Policy

`main` から作業ブランチを切り、PR で `main` にマージする。
