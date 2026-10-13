# v14-c-remove-isolated-workspaces Implementation Packet

## Goal

isolated workspace mode を削除し、すべての run がプロジェクトルートを共有するようにする。

## Why

isolated はモデルが opt-in したときしか効かず、直接のファイルツールや子 run から漏れている。一方で restore、エスカレーション、cancellation、GUI まで経路が広がり、維持コストに見合わない。worktree での作業はエージェントの運用で十分に回っている（ADR 0031）。

## Scope

- **run の設定:** `run.rs` の `WorkspaceMode` / `MergeMode`、`RunConfig.workspace_mode` / `workspace_branch` / `merge_mode` を削除する。`WorkspaceInspection` は `active_root` だけにし、`snapshot_root` のような worktree 前提のヘルパーも外す。
- **delegate:** `meta/delegation.rs`、`meta/specs.rs`、`specs/contracts.rs` から `workspace_mode` / `workspace_branch` を削除する。`agent_loop/benchmark.rs` の schema から外す処理と、`benchmark.rs` の shared 限定の検査は不要になるので整理する。
- **worktree:** `workspace.rs` の `Project` / `WorktreeManager` / `OwnedWorktree` を削除する。`agent_loop.rs` の `setup_isolated_workspace` / `attach_adopted_workspace` / `cleanup_worktree` と、`finalization.rs`、`escalation/handoff.rs`、`restore.rs`、`restore/escalation.rs`、`runtime/cancellation.rs`、`runtime/chat_restore.rs` の worktree の扱いを削除する。`runtime.rs` の `WorkspaceContext` から manager を外し、`set_project_root` での作り直しをやめる。
- **sandbox の組み立て:** `network.rs` の `isolated_mounts` を削除する。`SandboxFactory`、`BwrapFactory`、mount の構造体は shared run が使うので残し、`IsolatedMounts` を `WorkspaceMounts` などに改名する。`compose.rs` の `WorkspaceSeam` から `Project` を外す（git repository の検証が worktree のためだけなら、それも外す）。
- **GUI / workspace-ui:** `base_context/preview.rs` の isolated 分岐、context inspector の Isolated worktree チェックボックス（`model/context_inspector.rs`、`panes/context_inspector/controls.rs`）、`app/frame.rs` の diff ルートの選択、`model/project_bridge.rs`、サイドバーの branch / worktree 表示（`panes/sidebar/threads.rs`、`workspace-ui/src/thread.rs`）、demo の isolated worker（`model/demo.rs`、`bin/evorch-gui.rs`）、`workspace-ui/src/project.rs` の `Membership::RuntimeWorktree` を削除する。
- **prompt:** `orchestration/prompts.rs` に isolated の指示が残っていれば削除する（大半は v14-b で消える）。tool schema の変更に合わせて prompt golden と cache 契約のテストを更新する。

### 採用・却下案と理由

- 外部の許可ディレクトリにも worktree を作る案は却下した（ADR 0031）。worktree が run の数 × リポジトリの数に増え、merge と diff を複数のリポジトリに対応させる必要がある。
- `IsolatedMounts` を丸ごと削除する案は却下する。shared run の sandbox の組み立てに使われており、ADR 0032 / 0033 で許可ディレクトリや scratch の mount を加える受け皿になる。

### 名前の注意

shell の `sandbox_access: "isolated"`（`crates/tools/src/tools/shell.rs`）と、その値を説明する `docs/harness-runtime-improvements.md` の表は、sandbox の中で実行するという意味で、この unit の対象ではない。grep で `isolated` を一括置換しないこと。

## Out of scope

shell の `sandbox_access` と昇格の経路、許可ディレクトリ（ADR 0032）、ファイルツールの境界と scratch（ADR 0033）。

## Verification

### Unit

- delegate の schema に `workspace_mode` / `workspace_branch` が含まれないこと（既存の tool spec テストの更新）。
- `WorkspaceInspection` と context inspector の preview が `active_root` だけで組み立てられること。

### Integration → build

- delegate した Worker がプロジェクトルートで書き込めること。エスカレーションと復元が worktree なしで成り立つこと（`escalation_handoff.rs`、`restore*.rs`、`cancel_semantics.rs`、`stop_semantics.rs` を shared の前提に直す）。
- `workspace_runtime.rs`、`workspace_cleanup.rs`、`workspace_sandbox_e2e.rs` の worktree 部分、GUI の `sidebar_headless.rs` の worktree 表示のテストは削除する。shared の sandbox を検証している部分は残す。
- shell の昇格のテスト（`shell_escalation_*.rs`）が変わらず pass すること。
- `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更 crate の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check`。

### 未確認事項

- `compose.rs` の `WorkspaceSeam::production` がプロジェクトルートを git repository として検証しているのが worktree のためだけか。ほかに git repository であることを前提にした処理（diff など）が runtime 側にあれば、その検証は残す。
