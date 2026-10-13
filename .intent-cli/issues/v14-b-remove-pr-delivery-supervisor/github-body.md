# ADR 0031 ②: 旧 PR delivery supervisor の削除

## Goal

goal から PR、CI、Reviewer、merge 承認、closeout までを進める旧 `GoalSupervisor` と、その PR delivery の経路を削除する。thread goals、thread todos、team task の durable 状態のように、ほかから使われている部分は残す。

## Why This Slice Exists Now

汎用 goal は ADR 0028 の thread goals に移り、本番で新しい supervisor goal は作られていない。それでも supervisor は起動時に組み立てられ、`finish` の gate を登録し、Reviewer を isolated workspace で起動する経路を持ち続けている。ADR 0031 で isolated workspace を削除するには、先にこの経路を外す必要がある。

## Current Observed State

- `SupervisorHandle::create_goal` を呼ぶのはテストだけ。GUI の `SubmitGoal` は thread goals に入る。
- `evorch-gui.rs` が `GoalSupervisor::spawn` に `ShellDeliveryAdapter` を渡して起動し、永続化済みの goal を `adopt` で引き継ぐ。`runtime_sink.rs` は `DecideMerge` / `PauseGoal` / `ResumeGoal` / `CancelGoal` を supervisor に転送する。
- `GoalSupervisor::spawn` が `with_goal_gate` で gate を登録し、thread goal を持たない run の `finish` がその gate を通る（`crates/runtime/src/meta/mod.rs`）。
- `submit_review` meta op と `ReviewResult` は supervisor の review ループだけが使う。Reviewer role の capability に `submit_review` が入っている。thread goals のレビューは `submit_goal_review` を使う。
- `OrchestratorEvent` は thread goals（`ThreadGoalUpdated`）、thread todos（`ThreadTodoUpdated`）、team task の run（`TaskProgressed` / `TaskCheckpoint`）とも共有されている。task の durable 状態の処理の一部は `supervisor/tasks.rs`・`stale.rs`・`task_admission.rs`・`budget.rs` と `ledger/durable.rs` にある。
- `OrchestrationConfig` の `max_review_rounds` は thread goals も使う。

## Accepted Baseline You May Assume

- イベントと goal ledger の保存形式は後方互換を保たなくてよい（AGENTS.md の Compatibility policy）。旧 supervisor の goal を引き継ぐ必要はない。
- isolated workspace 自体（`WorkspaceMode` など）の削除は v14-c で行う。この unit では supervisor から isolated を使う箇所を消すだけにする。
- ワークスペース snapshot の削除は v14-a で行う。

## Target Repo / Path / Part

- Target repo: `turtton/evorch`
- Target paths: `crates/runtime/` `crates/event-bus/` `crates/storage/` `crates/agents/` `crates/config/` `crates/gui/` `docs/` `intents/`
- Target part: `crates/runtime/src/orchestration/` の PR delivery 部分、`submit_review`、`finish` の gate、GUI の goal 操作と merge 承認、関連するイベント・型・設定・テスト。

## In Scope

- 最初に、`crates/runtime/src/orchestration/` を「PR delivery 専用（削除）」と「thread goals・thread todos・team task の durable 状態が使う（残す、または supervisor の外へ移す）」に分け、PR 本文に一覧を載せる。
- `GoalSupervisor` / `SupervisorHandle`、`DeliveryPort` と各 adapter、merge 承認、closeout、continuation、stall の nudge、PR の gate の削除。`evorch-gui.rs` での組み立てと goal の引き継ぎの削除。
- `submit_review`、`reviewer_channel`、`ReviewResult`、`reviewer_results` の削除と、Reviewer role の capability からの `submit_review` の除去。
- GUI の `DecideMerge` / `PauseGoal` / `ResumeGoal` / `CancelGoal` と merge 承認の通知の削除。
- PR delivery だけで使っていた `OrchestratorEvent` の variant と型、storage の projection、`OrchestrationConfig` の項目の削除（`max_review_rounds` は残す）。
- `crates/gui/docs/workbench-panels.md` の更新。durable の処理を移した場合は ADR 0026 への追記。

## Out Of Scope

isolated workspace の削除（v14-c）、ワークスペース snapshot の削除（v14-a）、thread goals の振る舞いの変更。

## Acceptance Criteria

1. `GoalSupervisor` / `SupervisorHandle`、`DeliveryPort` と各 adapter、merge 承認、closeout、continuation、stall の nudge、PR の gate が削除され、evorch-gui はそれらを組み立てない。
2. `submit_review`、`reviewer_channel`、`ReviewResult`、`reviewer_results` が削除され、Reviewer role の capability から `submit_review` が外れている。
3. GUI の goal 操作の command と merge 承認の通知が削除され、`/goal` はこれまでどおり thread goals に入る。
4. thread goals、thread todos、team task の durable 状態（保存・復元・GUI 表示を含む）がこれまでどおり動く。
5. PR delivery だけで使っていたイベント・型・projection・設定項目が削除されている。`max_review_rounds` は残る。
6. 旧 supervisor の goal イベントが残る DB で起動しても GUI が起動できる。
7. supervisor だけを検証していたテストは削除し、setup に supervisor を使っていただけのテストは supervisor なしで動く。
8. `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更範囲の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check` が pass する。

## Verification

1. Integration: thread goals の self-check とレビュー、thread todos、team task の durable 状態の保存・復元が pass すること。
2. Integration: 旧 goal イベントを含む DB で GUI の起動経路（headless）が成功すること。
3. Build / lint: 上記のコマンド一式。tool schema が変わる（`submit_review` の削除）ので、prompt golden と cache 契約のテストを更新する。

## Related Links

- `intents/evorch/decisions/0031-remove-isolated-workspaces.md`
- `intents/evorch/decisions/0028-thread-goals-and-completion-checks.md`
- `intents/evorch/decisions/0026-durable-execution-substrate.md`
- `intents/evorch/features/orchestration/overview.md`

## Base Branch Policy

`main` から作業ブランチを切り、PR で `main` にマージする。
