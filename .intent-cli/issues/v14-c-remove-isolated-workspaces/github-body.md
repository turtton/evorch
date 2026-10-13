# ADR 0031 ③: isolated workspace の削除

## Goal

runtime が委譲した run ごとに git worktree を作る isolated workspace mode を削除し、すべての run がプロジェクトルートを共有するようにする。worktree での作業は、エージェントが skill の手順で行う運用に任せる。

## Why This Slice Exists Now

isolated になるのは、モデルが delegate に `workspace_mode: "isolated"` を付けたときと、旧 supervisor の Reviewer だけである。直接のファイルツールは本体プロセスで動くので隔離が漏れており、isolated な run の子 run もプロジェクトルートを引き継ぐ。それでも restore、エスカレーション、cancellation、GUI まで経路が広がっている。実際の運用では、エージェントが自分でプロジェクトの中に worktree を作って作業しており、問題なく回っている（ADR 0031）。

## Current Observed State

- `WorkspaceMode` / `MergeMode` と `RunConfig.workspace_mode` / `workspace_branch` / `merge_mode` が `crates/runtime/src/run.rs` にある。既定は `Shared`。
- `workspace.rs` の `WorktreeManager` / `OwnedWorktree` が `<root>/.evorch/worktrees/<run>` と branch `evorch/task/<run>` を作り、`agent_loop.rs`、`finalization.rs`、`escalation/handoff.rs`、`restore.rs`、`restore/escalation.rs`、`runtime/cancellation.rs`、`runtime/chat_restore.rs` が扱う。
- `network.rs` の `isolated_mounts` が worktree 用の git metadata の mount を作る。`SandboxFactory` / `IsolatedMounts` / `BwrapFactory` は shared run の sandbox の組み立てにも使われている。
- GUI の context inspector に Isolated worktree のチェックボックスがあり、diff・サイドバー・demo が worktree を前提にした表示を持つ。workspace-ui に `Membership::RuntimeWorktree` がある。

## Accepted Baseline You May Assume

- 旧 PR delivery supervisor は v14-b で削除済み。
- run スナップショットと復元記述子の保存形式は後方互換を保たなくてよい（AGENTS.md の Compatibility policy）。
- **名前の注意**: shell の引数 `sandbox_access: "isolated"` は「sandbox の境界の中で実行する」という意味で、この unit で削除する isolated workspace とは関係がない。削除作業で巻き込まないこと（ADR 0031、ADR 0033）。

## Target Repo / Path / Part

- Target repo: `turtton/evorch`
- Target paths: `crates/runtime/` `crates/workspace-ui/` `crates/gui/` `crates/event-bus/` `docs/` `intents/`
- Target part: isolated workspace mode とその worktree の作成・引き渡し・後始末・表示。

## In Scope

- `WorkspaceMode`、`MergeMode`、`RunConfig` の関連フィールド、delegate の `workspace_mode` / `workspace_branch` 引数の削除。tool schema の変更に合わせた prompt golden と cache 契約のテストの更新。
- `workspace.rs` の `WorktreeManager` / `OwnedWorktree` / `Project`、`isolated_mounts`、`setup_isolated_workspace` / `attach_adopted_workspace` / `cleanup_worktree`、エスカレーションでの worktree の引き渡し、復元記述子の worktree 情報、cancellation と chat restore での後始末の削除。`set_project_root` から worktree manager の作り直しを外す。
- `SandboxFactory` と mount の構造体は残し、名前から isolated を外す（例: `IsolatedMounts` → `WorkspaceMounts`）。
- `WorkspaceInspection` を `active_root` だけにし、GUI（diff のルート選択、サイドバーの branch / worktree 表示、context inspector のチェックボックス、demo）と workspace-ui（`Membership::RuntimeWorktree`、thread の `branch` / `worktree_path`）を追従させる。

## Out Of Scope

shell の `sandbox_access` とその昇格の経路、許可ディレクトリのマウント（ADR 0032）、ファイルツールの境界（ADR 0033）。

## Acceptance Criteria

1. `WorkspaceMode`、`MergeMode`、`RunConfig` の関連フィールド、delegate の `workspace_mode` / `workspace_branch` 引数が削除され、すべての run がプロジェクトルートで動く。
2. worktree の作成・引き渡し・復元・後始末の経路が削除されている。
3. shared run の sandbox の組み立てに使う factory と mount の構造体は残り、名前から isolated が外れている。
4. `WorkspaceInspection` は `active_root` だけを持ち、GUI と workspace-ui から isolated の表示と入力が消えている。
5. shell の `sandbox_access: "isolated"` と昇格の経路は変更されていない。
6. isolated だけを検証していたテストは削除し、ほかの検証も兼ねるテストは shared の前提に直す。
7. `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更範囲の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check` が pass する。

## Verification

1. Integration: delegate した Worker がプロジェクトルートで書き込み、エスカレーションと復元が worktree なしで成り立つこと。
2. Integration: shell の `sandbox_access: "unsandboxed"` / `"network"` の昇格テストが変わらず pass すること。
3. Build / lint: 上記のコマンド一式。

## Related Links

- `intents/evorch/decisions/0031-remove-isolated-workspaces.md`
- `intents/evorch/decisions/0027-restore-contract.md`
- `intents/evorch/features/agent-runtime-kernel/overview.md`
- `intents/evorch/features/tools-sandbox/overview.md`

## Base Branch Policy

`main` から作業ブランチを切り、PR で `main` にマージする。
