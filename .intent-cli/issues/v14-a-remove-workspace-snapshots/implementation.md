# v14-a-remove-workspace-snapshots Implementation Packet

## Goal

書き込み系ツールの前に撮っているワークスペース snapshot とロック、`/undo` / `/redo` を削除し、shell のロック待ちをなくす。

## Why

ロックの目的は `/undo` / `/redo` の run 単位の巻き戻しだけである。shell を書き込みありとして扱うため、読み取りだけのコマンドや裏で動くジョブまでロックを持ち、並列の run が日常的に待たされている。巻き戻しは git（commit、ブランチ、worktree の破棄）で足りる（ADR 0031）。

## Scope

- **runtime:** `agent_loop/snapshots.rs` を削除し、`tool_calls.rs` の `snapshot_before_tool` 呼び出し、ガードの `prepared_wave` での保持、shell への `retain_shell_call_guard` を外す。キャンセル待ちの `select!` も snapshot 待ちがなくなるので整理する。`AgentRuntime` の `snapshots` フィールド、`with_snapshots`、`restore_snapshot` を削除する。
- **snapshot モジュール:** `snapshot/service.rs`（と `service/tests.rs`）、`snapshot/history.rs` を削除する。`SnapshotStore` は benchmark のために残し、`capture` / `restore` は使われなくなるので削除する。`diff` と `snapshot/benchmark.rs` は残す。
- **tools:** `ToolExecutor::retain_shell_call_guard`、`Tool::retain_shell_call_guard`、shell の実装と保持していたガードの表を削除する。
- **event-bus:** `EventKind::Snapshot` / `SnapshotEvent`、`LifecycleEvent::WorkspaceWaitChanged`、`WorkspaceWait`、`WorkspaceLockHolder` を削除し、`fencing.rs`、`otel/span/lifecycle.rs`、`lib.rs` の再公開を追従させる。
- **storage:** `entity.rs` の `Snapshot` の検査と `projection.rs` の match を削除する。
- **GUI:** `/undo` / `/redo` の slash 定義と処理（`model/composer.rs`、`app/composer.rs`）、`WorkbenchCommand::RestoreSnapshot`、`LoopEvent::SnapshotRestored`（`model/commands.rs`、`app/actions.rs`、`runtime_sink.rs`）、`model/workspace_wait.rs` と telemetry・サイドバーの待ち表示、`EventKind::Snapshot` を網羅していた match を削除する。`evorch-gui.rs` の `SnapshotService` 組み立てを外す。
- **Docs:** `docs/harness-validation.md`、`docs/harness-runtime-improvements.md`。

### 採用・却下案と理由

- ロックの単位を git worktree や cwd ごとに細かくする案は却下する。同じ worktree の中の shell 同士の待ちは残り、`gh pr checks` のような読み取りだけのコマンドが待つ問題は解決しない（ADR 0031）。
- undo を `write` / `edit` だけに絞って残す案は、利用者との相談で採らなかった。巻き戻しは git に任せる。
- `SnapshotStore` を丸ごと消す案は却下する。benchmark の記録・再生が `capture_all` / `restore_all` / `diff` を使っている。

## Out of scope

isolated workspace、旧 PR delivery supervisor、benchmark の snapshot 形式。

## Verification

### Unit

- shell / write / edit の実行経路にロック取得が残っていないこと（コンパイルで担保されるので、専用テストは追加しない）。

### Integration → build

- 裏で動く shell ジョブを持つ run があるときに、別の run の `write` / `shell` が待たされずに完了すること。完了はイベントで待つ（test-policy）。
- `parallel_tool_calls.rs`、`ownership_commands.rs`、`shell_jobs_integration.rs` のうち snapshot を前提にした部分を直す。`runtime/tests/snapshots.rs`、`shell_jobs_integration/workspace_wait.rs`、`event-bus/tests/workspace_wait.rs` は削除する。
- benchmark の記録・再生のテスト。
- `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更 crate の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check`。

### 未確認事項

- `shell_jobs_integration.rs` のうち、ジョブの刈り取りとロック解放の順序だけを検証しているテストと、ジョブの寿命そのものを検証しているテストの切り分けは、実装時に確認する。後者は残す。
