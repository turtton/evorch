use super::*;

#[test]
fn replay_routes_retry_to_goal_id_when_task_ids_overlap() {
    // Given: both goals own the same task ID through distinct attached runs.
    let mut other = created();
    if let OrchestratorEvent::GoalCreated {
        goal_id,
        root_run_id,
        ..
    } = &mut other
    {
        *goal_id = "goal-2".into();
        *root_run_id = "other-run".into();
    }
    let events = [
        created(),
        other,
        OrchestratorEvent::TaskProgressed {
            task_id: "shared-task".into(),
            run_id: "run-root".into(),
            progress: serde_json::json!({"done": 1}),
            reason: "advanced".into(),
        },
        OrchestratorEvent::TaskProgressed {
            task_id: "shared-task".into(),
            run_id: "other-run".into(),
            progress: serde_json::json!({"done": 2}),
            reason: "advanced".into(),
        },
        OrchestratorEvent::TaskRetryScheduled {
            goal_id: "goal-2".into(),
            task_id: "shared-task".into(),
            attempt: 2,
            reason: "transient".into(),
            new_run_id: "retry-run".into(),
        },
    ];
    let before = GoalLedger::replay_checked(events[..4].iter()).expect("valid setup");
    // When: replaying a retry addressed only to goal-2.
    let ledgers = GoalLedger::replay_checked(events.iter()).expect("no unresolved event");
    // Then: goal-1 is unchanged and goal-2 owns the retry and new run.
    assert_eq!(ledgers["goal-1"].snapshot(), before["goal-1"].snapshot());
    let goal = ledgers["goal-2"].snapshot();
    assert_eq!(goal.task_runs["shared-task"], "retry-run");
    assert_eq!(goal.task_attempts["shared-task"], 2);
    assert_eq!(
        goal.task_retries,
        vec![(
            "shared-task".into(),
            2,
            "transient".into(),
            "retry-run".into()
        )]
    );
    let mut retry = before["goal-2"].snapshot().attached_runs[0].clone();
    retry.run_id = "retry-run".into();
    assert_eq!(goal.attached_runs.last(), Some(&retry));
}

#[test]
fn replay_rejects_duplicate_creation() {
    // Given: a duplicate creation would reset already-applied state.
    let events = [created(), created()];
    // When: replaying the corrupted stream.
    let result = GoalLedger::replay_checked(events.iter());
    // Then: the duplicate is reported instead of replacing the ledger.
    assert_eq!(result, Err(vec![LedgerError::DuplicateCreation]));
}

#[test]
fn replay_rejects_unattached_task_events() {
    // Given: the run has never been attached to this goal.
    let events = [
        created(),
        OrchestratorEvent::TaskProgressed {
            task_id: "orphan-task".into(),
            run_id: "orphan-run".into(),
            progress: serde_json::json!({"done": 1}),
            reason: "unknown".into(),
        },
    ];
    // When: replaying the event stream.
    let errors = GoalLedger::replay_checked(events.iter()).expect_err("orphan rejected");
    // Then: ownership failure is surfaced, not silently dropped.
    assert_eq!(
        errors,
        vec![LedgerError::UnresolvedEvent(format!("{:?}", events[1]))]
    );
}

#[test]
fn replay_partial_keeps_valid_goals_and_collects_unresolvable_events() {
    // Given: one valid goal plus a chat-run boundary progression that no goal can
    // own (identity-less worker runs emit task_id == run_id and never get
    // RunAttached; such rows already exist in real user databases).
    let events = [
        created(),
        OrchestratorEvent::TaskProgressed {
            task_id: "run-chat".into(),
            run_id: "run-chat".into(),
            progress: serde_json::json!({"status": "running", "attempts": 0}),
            reason: "task execution boundary".into(),
        },
    ];
    // When: replaying tolerantly over external, user-owned durable input.
    let (ledgers, errors) = GoalLedger::replay_partial(events.iter());
    // Then: the valid goal survives and the stray event is reported, not fatal.
    assert!(ledgers.contains_key("goal-1"));
    assert_eq!(errors.len(), 1);
    assert!(matches!(errors[0], LedgerError::UnresolvedEvent(_)));
    // And: the strict API still rejects the same history (contract unchanged).
    assert!(GoalLedger::replay_checked(events.iter()).is_err());
}

#[test]
fn replay_routes_task_progress_to_only_its_attached_goal() {
    // Given: two goals with disjoint root runs.
    let mut other = created();
    if let OrchestratorEvent::GoalCreated {
        goal_id,
        root_run_id,
        ..
    } = &mut other
    {
        *goal_id = "goal-2".into();
        *root_run_id = "other-run".into();
    }
    let events = [
        created(),
        other,
        OrchestratorEvent::TaskProgressed {
            task_id: "task-2".into(),
            run_id: "other-run".into(),
            progress: serde_json::json!({"done": 9}),
            reason: "advanced".into(),
        },
    ];
    // When: resolving the goal-less event by its attached run.
    let ledgers = GoalLedger::replay_checked(events.iter()).expect("unique owner");
    // Then: only the owning goal receives the progress.
    assert!(ledgers["goal-1"].snapshot().task_progress.is_empty());
    assert_eq!(
        ledgers["goal-2"].snapshot().task_progress["task-2"],
        serde_json::json!({"done": 9})
    );
}
