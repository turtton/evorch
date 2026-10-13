# v14-c-remove-isolated-workspaces Review Context

## Position and scope

ADR 0031 の 3 段のうち ③。isolated workspace だけを扱う。旧 PR delivery supervisor（v14-b）とワークスペース snapshot（v14-a）は削除済みの前提。許可ディレクトリ（ADR 0032）とファイルツールの境界（ADR 0033）がないことを不足として扱わない。

## Intent references

- `intents/evorch/decisions/0031-remove-isolated-workspaces.md`
- `intents/evorch/decisions/0027-restore-contract.md`
- `intents/evorch/features/agent-runtime-kernel/overview.md`
- `intents/evorch/features/tools-sandbox/overview.md`

## Review focus

### 名前の取り違え

- shell の `sandbox_access: "isolated"`、その昇格の経路、`docs/harness-runtime-improvements.md` の表を消したり改名したりしていないか。

### 残すべきもの

- shared run の sandbox の組み立て（`SandboxFactory`、`BwrapFactory`、改名した mount の構造体）が残り、shell の mount が以前と同じか。

### 消し残し

- `WorkspaceMode`、`workspace_branch`、`OwnedWorktree`、`.evorch/worktrees`、`evorch/task/` の参照が残っていないか。
- エスカレーション、復元、cancellation、chat restore に worktree の後始末の分岐が残っていないか。
- GUI の Isolated worktree チェックボックスと、branch / worktree の表示が消えているか。

### tool schema と cache

- delegate の schema の変更が prompt golden と cache 契約のテストに反映されているか。

## Acceptance-to-evidence map

packet.yaml の acceptance_criteria の各項目に対応するテスト名を PR 本文に列挙する。
