use std::collections::BTreeSet;

use event_bus::{CloseoutStep, GoalStage, GoalState, OrchestratorEvent, RunPurpose};
use evorch_runtime::orchestration::ledger::{GoalLedger, LedgerError, OrchestrationSettings};
use runtime as evorch_runtime;

#[path = "goal_state_machine/replay_errors.rs"]
mod replay_errors;

fn created() -> OrchestratorEvent {
    OrchestratorEvent::GoalCreated {
        goal_id: "goal-1".into(),
        session_id: "session-1".into(),
        project_id: "evorch".into(),
        thread_id: "thread-1".into(),
        goal: "implement issue #73".into(),
        references: Vec::new(),
        constraints: vec!["pure".into()],
        repo: "turtton/evorch".into(),
        base_ref: "main".into(),
        root_run_id: "run-root".into(),
    }
}

fn ledger() -> GoalLedger {
    GoalLedger::new(&created())
}

#[test]
fn replay_propagates_ledger_errors_instead_of_swallowing() {
    // Given: two invalid transitions in an otherwise created goal.
    let events = [
        created(),
        OrchestratorEvent::GoalStateChanged {
            goal_id: "goal-1".into(),
            from: GoalState::Paused,
            to: GoalState::Active,
            reason: "conflicting source".into(),
        },
        OrchestratorEvent::GoalStateChanged {
            goal_id: "goal-1".into(),
            from: GoalState::Active,
            to: GoalState::Complete,
            reason: "missing closeout".into(),
        },
    ];
    // When: replay validates every event.
    let errors = GoalLedger::replay_checked(events.iter()).expect_err("invalid history");
    // Then: both failures survive in input order.
    assert_eq!(
        errors,
        vec![
            LedgerError::StateConflict {
                current: GoalState::Active,
                event_from: GoalState::Paused,
            },
            LedgerError::CloseoutIncomplete
        ]
    );
}

#[test]
fn replay_after_task_lifecycle_events_reconstructs_durable_state() {
    // Given: an attached worker reports progress, budget, retry and staleness.
    let events = [
        created(),
        OrchestratorEvent::RunAttached {
            goal_id: "goal-1".into(),
            run_id: "worker-1".into(),
            parent_run_id: Some("run-root".into()),
            role: "worker".into(),
            purpose: RunPurpose::Implement,
        },
        OrchestratorEvent::TaskProgressed {
            task_id: "task-1".into(),
            run_id: "worker-1".into(),
            progress: serde_json::json!({"done": 3}),
            reason: "advanced".into(),
        },
        OrchestratorEvent::TaskCheckpoint {
            task_id: "task-1".into(),
            run_id: "worker-1".into(),
            tool_call_count: 2,
            cumulative_input_tokens: 100,
            cumulative_output_tokens: 20,
            elapsed_ms: 30,
        },
        OrchestratorEvent::TaskCheckpoint {
            task_id: "task-1".into(),
            run_id: "worker-1".into(),
            tool_call_count: 4,
            cumulative_input_tokens: 250,
            cumulative_output_tokens: 50,
            elapsed_ms: 80,
        },
        OrchestratorEvent::TaskRetryScheduled {
            task_id: "task-1".into(),
            attempt: 1,
            reason: "transient".into(),
            new_run_id: "worker-2".into(),
        },
        OrchestratorEvent::TaskStaleMarked {
            task_id: "task-1".into(),
            run_id: "worker-2".into(),
            last_heartbeat_ns: 123,
        },
    ];
    // When: reconstructing the durable history.
    let replayed = GoalLedger::replay_checked(events.iter()).expect("valid history");
    let snapshot = replayed["goal-1"].snapshot();
    // Then: all task state and retry linkage are preserved.
    assert_eq!(
        snapshot.task_progress["task-1"],
        serde_json::json!({"done": 3})
    );
    assert_eq!(
        snapshot.task_checkpoints["task-1"],
        vec![(2, 100, 20, 30), (4, 250, 50, 80)]
    );
    assert_eq!(snapshot.task_attempts["task-1"], 1);
    assert_eq!(
        snapshot.task_retries,
        vec![("task-1".into(), 1, "transient".into(), "worker-2".into())]
    );
    assert_eq!(
        snapshot.stale_marks,
        vec![("task-1".into(), "worker-2".into(), 123)]
    );
    let retry = snapshot.attached_runs.last().expect("retry run");
    assert_eq!(retry.run_id, "worker-2");
    assert_eq!(retry.parent_run_id.as_deref(), Some("run-root"));
    assert_eq!(retry.role, "worker");
    assert_eq!(retry.purpose, RunPurpose::Implement);
}

fn ledger_in(state: GoalState) -> GoalLedger {
    let mut ledger = ledger();
    match state {
        GoalState::Active => {}
        GoalState::Paused | GoalState::Blocked | GoalState::Cancelled => {
            apply_transition(&mut ledger, state);
        }
        GoalState::Complete => {
            ledger
                .apply(&OrchestratorEvent::GoalStageChanged {
                    goal_id: "goal-1".into(),
                    from: GoalStage::Implementing,
                    to: GoalStage::Closeout,
                })
                .expect("stage change");
            for step in [
                CloseoutStep::WorkerClaim,
                CloseoutStep::ResultSummary,
                CloseoutStep::WorkerComplete,
            ] {
                ledger
                    .apply(&OrchestratorEvent::CloseoutStepRecorded {
                        goal_id: "goal-1".into(),
                        step,
                        ok: true,
                        artifact_ref: None,
                        detail: "ok".into(),
                    })
                    .expect("closeout step");
            }
            apply_transition(&mut ledger, GoalState::Complete);
        }
    }
    ledger
}

fn apply_transition(ledger: &mut GoalLedger, to: GoalState) {
    let event = ledger
        .transition(to, "test transition")
        .expect("transition must be valid");
    ledger.apply(&event).expect("event must apply");
}

#[test]
fn active_pause_resume_round_trip() {
    let mut ledger = ledger();

    apply_transition(&mut ledger, GoalState::Paused);
    assert_eq!(ledger.snapshot().state, GoalState::Paused);

    apply_transition(&mut ledger, GoalState::Active);
    assert_eq!(ledger.snapshot().state, GoalState::Active);
}

#[test]
fn paused_rejects_complete() {
    let mut ledger = ledger();
    apply_transition(&mut ledger, GoalState::Paused);

    assert!(ledger.transition(GoalState::Complete, "invalid").is_err());
}

#[test]
fn blocked_rejects_complete() {
    let mut ledger = ledger();
    apply_transition(&mut ledger, GoalState::Blocked);

    assert!(ledger.transition(GoalState::Complete, "invalid").is_err());
}

#[test]
fn cancelled_is_terminal_and_distinct_from_paused() {
    let mut cancelled = ledger();
    apply_transition(&mut cancelled, GoalState::Cancelled);
    assert_eq!(cancelled.snapshot().state, GoalState::Cancelled);
    assert!(cancelled.transition(GoalState::Active, "invalid").is_err());

    let mut paused = ledger();
    apply_transition(&mut paused, GoalState::Paused);
    assert_ne!(paused.snapshot().state, GoalState::Cancelled);
    let resumed = paused
        .transition(GoalState::Active, "resume")
        .expect("resume");
    assert_eq!(
        resumed,
        OrchestratorEvent::GoalStateChanged {
            goal_id: "goal-1".into(),
            from: GoalState::Paused,
            to: GoalState::Active,
            reason: "resume".into(),
        }
    );
    paused.apply(&resumed).expect("apply resume");
    assert_eq!(paused.snapshot().state, GoalState::Active);
}

#[test]
fn complete_requires_closeout_stage_and_success() {
    let mut ledger = ledger();
    assert!(ledger.transition(GoalState::Complete, "too early").is_err());

    ledger
        .apply(&OrchestratorEvent::GoalStageChanged {
            goal_id: "goal-1".into(),
            from: GoalStage::Implementing,
            to: GoalStage::Closeout,
        })
        .expect("stage change must apply");
    assert!(
        ledger
            .transition(GoalState::Complete, "missing steps")
            .is_err()
    );

    for (step, ok) in [
        (CloseoutStep::WorkerClaim, true),
        (CloseoutStep::ResultSummary, false),
        (CloseoutStep::WorkerComplete, true),
    ] {
        ledger
            .apply(&OrchestratorEvent::CloseoutStepRecorded {
                goal_id: "goal-1".into(),
                step,
                ok,
                artifact_ref: None,
                detail: "recorded".into(),
            })
            .expect("closeout event must apply");
    }
    assert!(
        ledger
            .transition(GoalState::Complete, "failed step")
            .is_err()
    );

    ledger
        .apply(&OrchestratorEvent::CloseoutStepRecorded {
            goal_id: "goal-1".into(),
            step: CloseoutStep::ResultSummary,
            ok: true,
            artifact_ref: Some("summary".into()),
            detail: "recovered".into(),
        })
        .expect("latest closeout result must apply");
    let event = ledger
        .transition(GoalState::Complete, "closeout complete")
        .expect("all closeout steps succeeded");
    ledger.apply(&event).expect("complete event must apply");
    assert_eq!(ledger.snapshot().state, GoalState::Complete);
}

#[test]
fn replay_rebuilds_identical_snapshot() {
    let events = [
        created(),
        OrchestratorEvent::RunAttached {
            goal_id: "goal-1".into(),
            run_id: "run-cont".into(),
            parent_run_id: Some("run-root".into()),
            role: "orchestrator".into(),
            purpose: RunPurpose::Continuation { epoch: 2 },
        },
        OrchestratorEvent::DeliverableBranchBound {
            goal_id: "goal-1".into(),
            branch: "evorch/task/run-1".into(),
            run_id: "run-worker".into(),
        },
        OrchestratorEvent::ContinuationDispatched {
            goal_id: "goal-1".into(),
            epoch: 2,
            trigger_run_id: "run-root".into(),
            new_run_id: "run-cont".into(),
            unmet: Vec::new(),
        },
        OrchestratorEvent::NudgeSent {
            goal_id: "goal-1".into(),
            run_id: "run-worker".into(),
            nudge_index: 1,
            message_id: "message-1".into(),
        },
    ];
    let mut incremental = GoalLedger::new(&events[0]);
    for event in &events[1..] {
        incremental.apply(event).expect("incremental apply");
    }

    let replayed = GoalLedger::replay_checked(events.iter()).expect("valid history");
    assert_eq!(
        replayed.get("goal-1").expect("replayed goal").snapshot(),
        incremental.snapshot()
    );
    assert_eq!(
        replayed["goal-1"].snapshot().dispatched_epochs,
        BTreeSet::from([2])
    );
}

#[test]
fn pause_suppresses_dispatch_predicate() {
    let mut ledger = ledger();
    assert!(ledger.can_dispatch_continuation(true, true, false, 8));

    apply_transition(&mut ledger, GoalState::Paused);
    assert!(!ledger.can_dispatch_continuation(true, true, false, 8));

    let inputs = ledger.gate_inputs(Some("head"), OrchestrationSettings::default());
    assert_eq!(inputs.max_review_rounds, 3);
}

#[test]
fn invalid_transition_matrix_25_pairs() {
    let states = [
        GoalState::Active,
        GoalState::Paused,
        GoalState::Blocked,
        GoalState::Complete,
        GoalState::Cancelled,
    ];

    for from in states {
        for to in states {
            let ledger = ledger_in(from);
            let allowed = matches!(
                (from, to),
                (GoalState::Active, GoalState::Paused)
                    | (GoalState::Active, GoalState::Blocked)
                    | (GoalState::Active, GoalState::Cancelled)
                    | (GoalState::Paused, GoalState::Active)
                    | (GoalState::Paused, GoalState::Cancelled)
                    | (GoalState::Blocked, GoalState::Active)
                    | (GoalState::Blocked, GoalState::Cancelled)
            );
            assert_eq!(
                ledger.transition(to, "matrix").is_ok(),
                allowed,
                "unexpected transition result: {from:?} -> {to:?}"
            );
        }
    }
}
