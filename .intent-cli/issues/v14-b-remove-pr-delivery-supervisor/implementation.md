# v14-b-remove-pr-delivery-supervisor Implementation Packet

## Goal

使われていない旧 PR delivery supervisor を削除し、thread goals・thread todos・team task の durable 状態だけを残す。

## Why

本番で新しい supervisor goal は作られていないが、supervisor は起動時に組み立てられ、`finish` の gate と isolated の Reviewer の経路を持ち続けている。isolated workspace の削除（v14-c）の前に外す（ADR 0031）。

## Scope

- **分類（最初の作業）:** `crates/runtime/src/orchestration/` の各ファイル（supervisor とその下の tasks / stale / task_admission / budget、ledger とその下の durable / replay、gate、review、reviewer_channel、delivery、shell_delivery、closeout、continuation、stall、approval、registry、prompts、types）を、「PR delivery 専用」と「thread goals・thread todos・team task の durable 状態が使う」に分ける。後者は supervisor の actor に依存しない形で残すか、適切な場所（例: team や durable task の側）へ移す。分類の結果を PR 本文に載せる。
- **runtime:** `GoalSupervisor` / `SupervisorHandle` / `SupervisorActor`、`DeliveryPort` と `ShellDeliveryAdapter` / `FixtureDeliveryAdapter`、merge 承認（`MergeApprovals` / `MergeBinding` / `ApprovedMerge`）、closeout、continuation、stall の nudge を削除する。`AgentRuntime::with_goal_gate` / `goal_gate` と `GoalGate`、`meta/mod.rs` の `finish` の gate 分岐を削除する。`submit_review`（registry / specs / contracts）、`reviewer_channel.rs`、`ReviewResult`、`reviewer_results` を削除する。`lib.rs` の再公開を整理する。
- **agents:** Reviewer role の capability から `submit_review` を外す。
- **event-bus / storage:** PR delivery 専用の `OrchestratorEvent` の variant（`GoalCreated`、`GoalStageChanged`、`DeliverableBranchBound`、`MergeApproval*`、`MergeExecuted`、`CloseoutStepRecorded`、`ReviewRoundStarted`、`RepairDispatched`、`StallDetected`、`NudgeSent`、`ContinuationDispatched` など）と型（`GoalStage`、`CiState`、`GateEvidence`、`GateRejection` など）を、分類の結果に従って削除する。`storage/src/projection/durable.rs` を追従させる。
- **config:** `OrchestrationConfig` から supervisor 専用の項目を削除する。`max_review_rounds` は thread goals が使うので残す（置き場所を thread goals 側へ移すかは実装時に判断し、移す場合は設定スキーマと docs/config を更新する）。
- **GUI:** `evorch-gui.rs` の supervisor 組み立て・goal の引き継ぎ・`orchestration_settings_or_default`、`runtime_sink.rs` の supervisor フィールドと goal 操作の転送、`WorkbenchCommand` の `DecideMerge` / `PauseGoal` / `ResumeGoal` / `CancelGoal`、merge 承認の通知、`model/durable_tasks/orchestrator.rs` の PR delivery 部分を削除する。demo の goal フロー（`model/demo.rs`）を thread goals に合わせるか削除する。
- **Docs / intents:** `crates/gui/docs/workbench-panels.md`。durable の処理を移した場合は ADR 0026 に追記する。

### 採用・却下案と理由

- supervisor を残して Reviewer だけ shared で起動する案は、利用者との相談で採らなかった。新しい goal が作られない経路を維持するコストに見合わない。
- 旧 goal ledger を読み込んで変換する移行層は作らない（Compatibility policy）。ただし起動は妨げない。

## Out of scope

isolated workspace の削除、ワークスペース snapshot の削除、thread goals の振る舞いの変更。

## Verification

### Unit

- Reviewer role の capability に `submit_review` が含まれないこと（既存の capability テストの更新）。
- `finish` が gate なしで完了すること。

### Integration → build

- thread goals（self-check、独立レビュー、`max_review_rounds`）、thread todos、team task の durable 状態の保存・復元のテスト。
- 旧 supervisor の goal イベントを含む DB で、GUI の起動経路（headless）が成功すること。
- supervisor 専用のテスト（`goal_supervisor_continuation.rs`、`goal_supervisor_stall.rs`、`goal_persistence.rs`、`goal_recovery.rs`、`merge_approval.rs`、`closeout_sequence.rs`、`shell_delivery_adapter.rs`、`orchestrator_loop_e2e.rs`、`orchestrator_flow.rs`、`review_loop.rs`、`review_recovery.rs`、`reviewer_transport.rs`、`finish_gate_table.rs`、`shell_merge_review_runtime.rs`、GUI の `orchestrator_loop_headless.rs`、`goal_restore_authority.rs`、`support/legacy_goal.rs` など）は削除する。`durable_admission.rs`、`durable_worker_boundary.rs`、`cancel_semantics.rs`、GUI の `chat_sink_runtime.rs`、`auto_write_mode.rs` など setup に supervisor を使っていただけのテストは、supervisor なしで動く形に直す。どれが専用かは分類の結果に従う。
- tool schema の変更に合わせて prompt golden と cache 契約のテストを更新する。
- `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更 crate の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check`。

### 未確認事項

- team task の durable 状態（`TaskProgressed` / `TaskCheckpoint`）が、supervisor の goal ledger なしでどこに保存・復元されるべきか。`agent_loop/durable.rs` のコメントは goal ledger の所有物になる前提で書かれているので、分類の段階で置き場所を決める。
- stale worker の検出（`supervisor/stale.rs`）と budget 診断の処理（`supervisor/budget.rs`）が、goal を持たない team task にも効いているか。効いていれば残す。
