use super::fixture::*;
use super::*;
use crate::{Role, RunConfig};
use event_bus::AgentRunPhase;
use providers::{ContentBlock, Message};
use std::sync::atomic::Ordering;

#[tokio::test]
async fn before_model_request_parks_without_consuming_durable_queue() {
    let fixture = Fixture::new(1);
    let run = fixture.start();
    fixture.runtime.request_quiesce();
    fixture
        .runtime
        .send_message_with_images(run, "queued".into(), Vec::new(), Some("queued-id".into()))
        .unwrap();
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
    for step in [StorageStep::Snapshot, StorageStep::Save, StorageStep::Flush] {
        let fixture = Fixture::new(1);
        let run = fixture.start();
        *fixture.runtime.shared.quiesce.fault.lock().unwrap() = Some(step);
        fixture.runtime.request_quiesce();
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
    let fixture = Fixture::new(1);
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
    for reason in [
        StopReason::OperatorStopped,
        StopReason::CliDisconnected,
        StopReason::Cancelled,
    ] {
        let fixture = Fixture::new(1);
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
        let phase = fixture.runtime.wait(run).await.unwrap();
        assert!(matches!(
            phase,
            AgentRunPhase::Stopped | AgentRunPhase::Error
        ));
        let intent = fixture.intent(run);
        assert_eq!(intent.stop_reason, Some(reason));
        assert!(!intent.active);
        assert_eq!(fixture.runtime.quiesce_state(), QuiesceState::Running);
    }
}

#[tokio::test]
async fn provider_admission_is_durable_once_and_quiesce_waits_for_its_actual_result() {
    let fixture = Fixture::with_admission(1, true);
    let run = fixture.start();
    fixture.model.admission_entered.notified().await;
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
    assert_eq!(fixture.model.admission_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 0);
    fixture.cleanup(run).await;
}

#[tokio::test]
async fn queued_admission_stop_is_durable_without_a_provider_or_a_loop() {
    for reason in [StopReason::OperatorStopped, StopReason::Cancelled] {
        let fixture = Fixture::new(1);
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
    let fixture = Fixture::new(1);
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
        .admit(next, fixture.runtime.shared.run_store.get(), false)
        .unwrap();
    let next = fixture.intent(run);
    assert!(next.initial_input.is_none());
    assert_eq!(next.inputs, vec![initial]);
    assert!(next.checkpoint.is_none());
}

#[tokio::test]
async fn setup_failure_retires_intent_before_terminal_publication() {
    let fixture = Fixture::new(1);
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
    let fixture = Fixture::new(1);
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
