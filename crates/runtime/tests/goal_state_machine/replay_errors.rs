use super::*;

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
