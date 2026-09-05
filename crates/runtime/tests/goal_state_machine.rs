use std::collections::BTreeSet;

use event_bus::{CloseoutStep, GoalStage, GoalState, OrchestratorEvent, RunPurpose};
use evorch_runtime::orchestration::ledger::{GoalLedger, OrchestrationSettings};
use runtime as evorch_runtime;

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
    assert!(paused.transition(GoalState::Active, "resume").is_ok());
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
    let events = vec![
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

    let replayed = GoalLedger::replay(events.iter());
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
