# v14-a-remove-workspace-snapshots Review Context

## Position and scope

ADR 0031 の 3 段のうち ①。ワークスペース snapshot、ロック、`/undo` / `/redo` だけを扱う。isolated workspace（v14-c）と旧 PR delivery supervisor（v14-b）が残っていることを不足として扱わない。

## Intent references

- `intents/evorch/decisions/0031-remove-isolated-workspaces.md`
- `intents/evorch/features/tools-sandbox/overview.md`
- `intents/evorch/features/gui-workbench/overview.md`

## Review focus

### ロック待ちの解消

- write / edit / shell の経路にワークスペースのロックや待ちが残っていないか。キャンセル待ちの `select!` が、snapshot 待ちを外したあとも中断を正しく扱うか。
- 裏で動く shell ジョブがあっても、別の run がすぐ実行できることをテストで示しているか。テストが時間待ちではなくイベントで完了を待っているか。

### benchmark

- `SnapshotStore` の `capture_all` / `restore_all` / `diff` と benchmark の記録・再生が壊れていないか。

### 削除の範囲

- `SnapshotEvent`、`WorkspaceWaitChanged` の削除で、storage・otel・GUI の match に取り残しがないか。
- `/undo` / `/redo` が補完候補・`/help`・slash 登録のどこにも残っていないか。
- 削除した機能だけを検証していたテストが残っていないか。逆に、ほかの検証も兼ねるテストまで消していないか。

## Acceptance-to-evidence map

packet.yaml の acceptance_criteria の各項目に対応するテスト名を PR 本文に列挙する。
