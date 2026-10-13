# v14-b-remove-pr-delivery-supervisor Review Context

## Position and scope

ADR 0031 の 3 段のうち ②。旧 PR delivery supervisor だけを扱う。`WorkspaceMode` などの isolated workspace 自体（v14-c）と、ワークスペース snapshot（v14-a）が残っていることを不足として扱わない。

## Intent references

- `intents/evorch/decisions/0031-remove-isolated-workspaces.md`
- `intents/evorch/decisions/0028-thread-goals-and-completion-checks.md`
- `intents/evorch/decisions/0026-durable-execution-substrate.md`
- `intents/evorch/features/orchestration/overview.md`

## Review focus

### 残すべきものを消していないか

- thread goals（`ThreadGoalUpdated`、`submit_goal_check` / `submit_goal_review`、`max_review_rounds`）と thread todos（`ThreadTodoUpdated`）が壊れていないか。
- team task の run が発行する `TaskProgressed` / `TaskCheckpoint` の保存・復元・GUI 表示が、supervisor なしでも成り立っているか。PR 本文の分類の一覧と、実際の削除範囲が一致しているか。

### 消し残し

- `finish` の gate、`submit_review`、Reviewer role の capability、merge 承認の通知、`DeliveryPort` が残っていないか。
- PR delivery 専用のイベント・型・設定項目が残っていないか。

### 起動と互換

- 旧 supervisor の goal イベントが残る DB で GUI が起動できるか。移行層を作っていないか。
- tool schema の変更が prompt golden と cache 契約のテストに反映されているか。

## Acceptance-to-evidence map

packet.yaml の acceptance_criteria の各項目に対応するテスト名を PR 本文に列挙する。
