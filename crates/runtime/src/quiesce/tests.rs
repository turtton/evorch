use super::fixture::*;
use super::*;
use crate::{Role, RunConfig};
use event_bus::AgentRunPhase;
use providers::{ContentBlock, Message};
use std::sync::atomic::Ordering;

#[tokio::test]
async fn before_model_request_parks_without_consuming_durable_queue() {
    for admission in [false, true] {
        before_model_request_parks_without_consuming_durable_queue_with_admission(admission).await;
    }
}

async fn before_model_request_parks_without_consuming_durable_queue_with_admission(
    admission: bool,
) {
    let fixture = Fixture::for_s2(1, admission);
    if admission {
        let weak = fixture.runtime.downgrade();
        *fixture
            .runtime
            .shared
            .admission_registration_hook
            .lock()
            .unwrap() = Some(std::sync::Arc::new(move |run| {
            let runtime = weak.upgrade().unwrap();
            runtime.request_quiesce();
            runtime
                .send_message_with_images(
                    run,
                    "queued".into(),
                    Vec::new(),
                    Some("queued-id".into()),
                )
                .unwrap();
        }));
    }
    let run = fixture.start();
    if admission {
        fixture.runtime.wait_admission(run).await.unwrap();
    } else {
        fixture.runtime.request_quiesce();
        fixture
            .runtime
            .send_message_with_images(run, "queued".into(), Vec::new(), Some("queued-id".into()))
            .unwrap();
    }
    fixture.runtime.wait_quiesced().await.unwrap();
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 0);
    let intent = fixture.intent(run);
    assert_eq!(intent.inputs.len(), 1);
    assert_eq!(intent.inputs[0].text, "queued");
    assert!(intent.active && intent.checkpoint.is_some());
    assert_eq!(intent.stop_reason, None);
    // A new admission is durable but cannot start a turn after quiescing.
    let queued = fixture.start();
    assert_eq!(fixture.intent(queued).wait, ResumeWait::AdmissionQueued);
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 0);
    fixture.cleanup(run).await;
}

#[tokio::test]
async fn provider_wait_and_parallel_partial_results_never_allow_exit() {
    let mut fixture = Fixture::new(2);
    let run = fixture.start();
    fixture.model.entered.notified().await;
    fixture.runtime.request_quiesce();
    let wait = fixture.runtime.wait_quiesced();
    tokio::pin!(wait);
    assert!(futures_util::poll!(&mut wait).is_pending());
    fixture.model.release.add_permits(1);
    let mut started = vec![
        fixture.entered.recv().await.unwrap(),
        fixture.entered.recv().await.unwrap(),
    ];
    started.sort();
    assert_eq!(started, vec![0, 1]);
    assert_eq!(fixture.intent(run).tool_intent.len(), 2);
    fixture.release[0].add_permits(1);
    assert_eq!(fixture.completed.recv().await, Some(0));
    assert!(futures_util::poll!(&mut wait).is_pending());
    assert_eq!(
        fixture.runtime.quiesce_state(),
        QuiesceState::WaitingForSafePoints
    );
    assert!(fixture.intent(run).checkpoint.is_none());
    fixture.release[1].add_permits(1);
    wait.await.unwrap();
    let intent = fixture.intent(run);
    assert_eq!(
        intent.checkpoint.as_ref().unwrap().boundary,
        crate::checkpoint::CheckpointBoundary::ToolsApplied
    );
    assert!(intent.tool_intent.is_empty());
    let record = storage::Database::open(&fixture.config)
        .unwrap()
        .run_context(&run.to_string())
        .unwrap()
        .unwrap();
    assert!(intent.checkpoint.unwrap().snapshot.available(Some(&record)));
    let messages: Vec<Message> = serde_json::from_str(&record.messages_json).unwrap();
    let ids: Vec<_> = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec!["call-0", "call-1"]);
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 1);
    fixture.cleanup(run).await;
}

#[tokio::test]
async fn snapshot_intent_and_flush_failures_never_publish_checkpointed() {
    for admission in [false, true] {
        snapshot_intent_and_flush_failures_never_publish_checkpointed_with_admission(admission)
            .await;
    }
}

async fn snapshot_intent_and_flush_failures_never_publish_checkpointed_with_admission(
    admission: bool,
) {
    for step in [StorageStep::Snapshot, StorageStep::Save, StorageStep::Flush] {
        let fixture = Fixture::for_s2(1, admission);
        let run = fixture.start();
        if admission {
            let weak = fixture.runtime.downgrade();
            *fixture
                .runtime
                .shared
                .admission_registration_hook
                .lock()
                .unwrap() = Some(std::sync::Arc::new(move |_| {
                let runtime = weak.upgrade().unwrap();
                *runtime.shared.quiesce.fault.lock().unwrap() = Some(step);
                runtime.request_quiesce();
            }));
        } else {
            *fixture.runtime.shared.quiesce.fault.lock().unwrap() = Some(step);
            fixture.runtime.request_quiesce();
        }
        assert!(fixture.runtime.wait_quiesced().await.is_err());
        assert!(matches!(
            fixture.runtime.quiesce_state(),
            QuiesceState::Failed { .. }
        ));
        assert!(fixture.intent(run).checkpoint.is_none());
        assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 0);
        fixture.cleanup(run).await;
    }
}

#[tokio::test]
async fn duplicate_admission_cannot_replace_the_active_incarnation() {
    for admission in [false, true] {
        duplicate_admission_cannot_replace_the_active_incarnation_with_admission(admission).await;
    }
}

async fn duplicate_admission_cannot_replace_the_active_incarnation_with_admission(admission: bool) {
    let fixture = Fixture::for_s2(1, admission);
    let run = fixture.start();
    let old = fixture.intent(run);
    assert!(
        fixture
            .runtime
            .admit_intent(
                run,
                "second-incarnation".into(),
                None,
                Role::Worker,
                "duplicate",
                &RunConfig::default(),
                false,
                true
            )
            .is_err()
    );
    assert_eq!(fixture.intent(run).incarnation, old.incarnation);
    fixture.runtime.request_quiesce();
    fixture.runtime.wait_quiesced().await.unwrap();
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 0);
    fixture.cleanup(run).await;
}

#[tokio::test]
async fn stop_reasons_are_independent_of_safe_point_quiesce() {
    for admission in [false, true] {
        stop_reasons_are_independent_of_safe_point_quiesce_with_admission(admission).await;
    }
}

async fn stop_reasons_are_independent_of_safe_point_quiesce_with_admission(admission: bool) {
    for reason in [
        StopReason::OperatorStopped,
        StopReason::CliDisconnected,
        StopReason::Cancelled,
    ] {
        let fixture = Fixture::for_s2(1, admission);
        let run = fixture.start();
        if reason == StopReason::CliDisconnected {
            fixture.runtime.record_cli_disconnected(run).unwrap();
        }
        if reason == StopReason::Cancelled {
            fixture.runtime.cancel(run).unwrap();
        } else {
            fixture
                .runtime
                .stop(run, crate::StopScope::SelfOnly)
                .unwrap();
        }
        let phase = fixture.runtime.wait(run).await;
        assert!(
            matches!(phase, Ok(AgentRunPhase::Stopped | AgentRunPhase::Error))
                || (admission && matches!(phase, Err(crate::RuntimeError::RunTerminated { .. })))
        );
        let intent = fixture.intent(run);
        assert_eq!(intent.stop_reason, Some(reason));
        assert!(!intent.active);
        assert_eq!(fixture.runtime.quiesce_state(), QuiesceState::Running);
    }
}

#[tokio::test]
async fn provider_admission_is_durable_once_and_quiesce_waits_for_its_actual_result() {
    for admission in [false, true] {
        provider_admission_is_durable_once_and_quiesce_waits_for_its_actual_result_with_admission(
            admission,
        )
        .await;
    }
}

async fn provider_admission_is_durable_once_and_quiesce_waits_for_its_actual_result_with_admission(
    admission: bool,
) {
    let fixture = Fixture::with_admission(1, admission);
    let run = fixture.start();
    if admission {
        fixture.model.admission_entered.notified().await;
    }
    assert!(fixture.intent(run).active);
    assert_eq!(fixture.intent(run).wait, ResumeWait::Admission);
    // An actual duplicate entry, not merely the schema helper, must not invoke
    // the provider's admission again while the first invocation is pending.
    assert_eq!(
        fixture
            .runtime
            .spawn_reserved(run, None, Role::Worker, "duplicate", RunConfig::default()),
        run
    );
    fixture.runtime.request_quiesce();
    let wait = fixture.runtime.wait_quiesced();
    tokio::pin!(wait);
    assert!(futures_util::poll!(&mut wait).is_pending());
    fixture.model.admission_release.add_permits(1);
    wait.await.unwrap();
    assert_eq!(
        fixture.model.admission_calls.load(Ordering::SeqCst),
        usize::from(admission)
    );
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 0);
    fixture.cleanup(run).await;
}

#[tokio::test]
async fn queued_admission_stop_is_durable_without_a_provider_or_a_loop() {
    for admission in [false, true] {
        queued_admission_stop_is_durable_without_a_provider_or_a_loop_with_admission(admission)
            .await;
    }
}

async fn queued_admission_stop_is_durable_without_a_provider_or_a_loop_with_admission(
    admission: bool,
) {
    for reason in [StopReason::OperatorStopped, StopReason::Cancelled] {
        let fixture = Fixture::for_s2(1, admission);
        fixture.runtime.quiesce().await.unwrap();
        let run = fixture.start();
        if reason == StopReason::Cancelled {
            fixture.runtime.cancel(run).unwrap();
        } else {
            fixture
                .runtime
                .stop(run, crate::StopScope::SelfOnly)
                .unwrap();
        }
        assert!(fixture.runtime.wait(run).await.is_err());
        let intent = fixture.intent(run);
        assert!(!intent.active);
        assert_eq!(intent.stop_reason, Some(reason));
        assert!(intent.initial_input.is_some());
        assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn history_only_readmission_keeps_an_initial_input_it_did_not_consume() {
    for admission in [false, true] {
        history_only_readmission_keeps_an_initial_input_it_did_not_consume_with_admission(
            admission,
        )
        .await;
    }
}

async fn history_only_readmission_keeps_an_initial_input_it_did_not_consume_with_admission(
    admission: bool,
) {
    let fixture = Fixture::for_s2(1, admission);
    fixture.runtime.quiesce().await.unwrap();
    let run = fixture.start();
    fixture
        .runtime
        .stop(run, crate::StopScope::SelfOnly)
        .unwrap();
    let old = fixture.intent(run);
    let initial = old.initial_input.clone().unwrap();
    let mut next = old;
    next.incarnation = "next-incarnation".into();
    next.active = true;
    next.wait = ResumeWait::Admission;
    next.initial_input = None;
    next.checkpoint = None;
    fixture
        .runtime
        .shared
        .quiesce
        .admit(next, fixture.runtime.shared.run_store.get(), false, None)
        .unwrap();
    let next = fixture.intent(run);
    assert!(next.initial_input.is_none());
    assert_eq!(next.inputs, vec![initial]);
    assert!(next.checkpoint.is_none());
}

#[tokio::test]
async fn setup_failure_retires_intent_before_terminal_publication() {
    for admission in [false, true] {
        setup_failure_retires_intent_before_terminal_publication_with_admission(admission).await;
    }
}

async fn setup_failure_retires_intent_before_terminal_publication_with_admission(admission: bool) {
    let fixture = Fixture::for_s2(1, admission);
    // Invalid team setup fails before LoopState/finalization exists.
    let run = fixture.runtime.delegate_background(
        Role::Worker,
        "work".into(),
        RunConfig {
            topology: crate::CoordinationTopology::DynamicTeam { max_workers: 1 },
            ..Default::default()
        },
    );
    assert_eq!(
        fixture.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Error
    );
    let intent = fixture.intent(run);
    assert!(!intent.active);
    assert_eq!(intent.stop_reason, Some(StopReason::Terminal));
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 0);
    fixture.runtime.quiesce().await.unwrap();
}

#[tokio::test]
async fn terminal_snapshot_failure_during_quiesce_never_allows_exit() {
    for admission in [false, true] {
        terminal_snapshot_failure_during_quiesce_never_allows_exit_with_admission(admission).await;
    }
}

async fn terminal_snapshot_failure_during_quiesce_never_allows_exit_with_admission(
    admission: bool,
) {
    let fixture = Fixture::for_s2(1, admission);
    let run = fixture.start();
    fixture.model.entered.notified().await;
    let conn = rusqlite::Connection::open(&fixture.config.db_path).unwrap();
    conn.execute_batch("CREATE TRIGGER reject_terminal_update BEFORE UPDATE ON run_contexts WHEN NEW.terminal_phase = 'Stopped' BEGIN SELECT RAISE(ABORT, 'injected terminal snapshot failure'); END;
        CREATE TRIGGER reject_terminal_insert BEFORE INSERT ON run_contexts WHEN NEW.terminal_phase = 'Stopped' BEGIN SELECT RAISE(ABORT, 'injected terminal snapshot failure'); END;").unwrap();
    fixture.runtime.request_quiesce();
    // Operator stop is allowed to end the task, but cannot certify a restart.
    fixture
        .runtime
        .stop(run, crate::StopScope::SelfOnly)
        .unwrap();
    assert_eq!(
        fixture.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Stopped
    );
    assert!(fixture.runtime.wait_quiesced().await.is_err());
    assert!(matches!(
        fixture.runtime.quiesce_state(),
        QuiesceState::Failed { .. }
    ));
}

#[tokio::test]
async fn normal_persistence_failure_does_not_quiesce_other_runs() {
    for admission in [false, true] {
        normal_persistence_failure_does_not_quiesce_other_runs_with_admission(admission).await;
    }
}

async fn normal_persistence_failure_does_not_quiesce_other_runs_with_admission(admission: bool) {
    let mut fixture = Fixture::for_s2(1, admission);
    let first = fixture.start();
    fixture.model.entered.notified().await;
    *fixture.runtime.shared.quiesce.fault.lock().unwrap() = Some(StorageStep::Save);
    assert!(
        fixture
            .runtime
            .send_message_with_images(
                first,
                "save fails".into(),
                vec![],
                Some("failed-input".into())
            )
            .is_err()
    );
    assert!(matches!(
        fixture.runtime.quiesce_state(),
        QuiesceState::Failed { .. }
    ));
    let other = fixture.start();
    // Admission is an observable durable gate: only an explicit restart request
    // may queue this unrelated run. No timeout is needed to detect the regression.
    assert_ne!(fixture.intent(other).wait, ResumeWait::AdmissionQueued);
    fixture.model.entered.notified().await;
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 2);
    // Already-running turns also pass their next post-tool/model safe point.
    fixture.model.release.add_permits(2);
    fixture.entered.recv().await.unwrap();
    fixture.entered.recv().await.unwrap();
    fixture.release[0].add_permits(2);
    fixture.completed.recv().await.unwrap();
    fixture.completed.recv().await.unwrap();
    while fixture.model.calls.load(Ordering::SeqCst) < 4 {
        fixture.model.entered.notified().await;
    }
    fixture.runtime.request_quiesce();
    assert!(fixture.runtime.wait_quiesced().await.is_err());
    fixture.cleanup(first).await;
    fixture.cleanup(other).await;
}

#[tokio::test]
async fn cancel_before_registration_is_durably_cancelled() {
    for admission in [false, true] {
        cancel_before_registration_is_durably_cancelled_with_admission(admission).await;
    }
}

async fn cancel_before_registration_is_durably_cancelled_with_admission(admission: bool) {
    let fixture = Fixture::for_s2(1, admission);
    let run = fixture.runtime.reserve_run_id();
    fixture.runtime.reserve_admission(run, None, None);
    fixture.runtime.cancel(run).unwrap();
    fixture
        .runtime
        .spawn_reserved(run, None, Role::Worker, "never start", RunConfig::default());
    assert!(fixture.runtime.wait(run).await.is_err());
    assert_eq!(fixture.intent(run).stop_reason, Some(StopReason::Cancelled));
    assert!(!fixture.intent(run).active);
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 0);
    assert!(fixture.runtime.wait(run).await.is_err());
}

#[tokio::test]
async fn queued_lane_admissions_are_busy_but_not_drained_by_restart() {
    for admission in [false, true] {
        queued_lane_admissions_are_busy_but_not_drained_by_restart_with_admission(admission).await;
    }
}

async fn queued_lane_admissions_are_busy_but_not_drained_by_restart_with_admission(
    admission: bool,
) {
    let fixture = Fixture::for_s2(1, admission);
    fixture.runtime.quiesce().await.unwrap();
    let lane = fixture.runtime.delegate_background(
        Role::Worker,
        "lane queued".into(),
        RunConfig {
            lane_scope: Some(event_bus::LaneScope::Lane),
            ..Default::default()
        },
    );
    let ordinary = fixture.start();
    assert_eq!(fixture.runtime.pending_lane_admissions(), vec![lane]);
    fixture.runtime.quiesce().await.unwrap();
    assert_eq!(fixture.runtime.pending_lane_admissions(), vec![lane]);
    assert_eq!(fixture.intent(lane).wait, ResumeWait::AdmissionQueued);
    assert!(fixture.intent(lane).active);
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 0);
    fixture.runtime.cancel(lane).unwrap();
    fixture.runtime.cancel(ordinary).unwrap();
    assert!(fixture.runtime.pending_lane_admissions().is_empty());
}

#[tokio::test]
async fn queued_lane_admissions_are_operator_drained_without_provider_tasks() {
    for admission in [false, true] {
        queued_lane_admissions_are_operator_drained_without_provider_tasks_with_admission(
            admission,
        )
        .await;
    }
}

async fn queued_lane_admissions_are_operator_drained_without_provider_tasks_with_admission(
    admission: bool,
) {
    let fixture = Fixture::for_s2(1, admission);
    fixture.runtime.quiesce().await.unwrap();
    let lane = fixture.runtime.delegate_background(
        Role::Worker,
        "lane queued".into(),
        RunConfig {
            lane_scope: Some(event_bus::LaneScope::Lane),
            ..Default::default()
        },
    );
    let ordinary = fixture.start();
    let input = fixture.intent(lane).initial_input;
    assert!(fixture.runtime.list_agents().is_empty());
    fixture.runtime.drain_lane_admissions().await.unwrap();
    let intent = fixture.intent(lane);
    assert_eq!(
        (intent.active, intent.stop_reason),
        (false, Some(StopReason::Cancelled))
    );
    assert_eq!(intent.initial_input, input);
    assert!(fixture.runtime.wait(lane).await.is_err());
    assert!(fixture.runtime.list_agents().is_empty());
    assert!(fixture.runtime.pending_lane_admissions().is_empty());
    assert_eq!(fixture.intent(ordinary).wait, ResumeWait::AdmissionQueued);
    assert!(fixture.intent(ordinary).active);
    assert_eq!(fixture.model.admission_calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 0);
    fixture.runtime.cancel(ordinary).unwrap();
}
