use super::*;
use crate::RunRestoreFailure;
use crate::thread_retention::tombstones_for_tests as tombstones;

fn assert_deleted<T: std::fmt::Debug>(result: Result<T, RuntimeError>) {
    match result {
        Err(RuntimeError::RunRestoreFailed {
            reason: RunRestoreFailure::ThreadDeleted(_),
            ..
        }) => {}
        other => panic!("expected a thread-deleted refusal, got {other:?}"),
    }
}

// Given: a terminal run and a guard that lists it (and a thread) as deleted
// When: the host tries to bind, resume, wake, continue, delegate, fork, deliver to or
// escalate from them
// Then: every entry refuses, nothing is restored or created, and a surviving run is unaffected.
#[tokio::test]
async fn every_entry_refuses_a_deleted_thread_or_run() {
    let fixture = Fixture::new();
    let runtime = &fixture.runtime;
    let gone = fixture.terminal().await;
    let kept = fixture.terminal().await;
    assert!(
        runtime
            .set_tombstone_guard(tombstones(&["gone-thread"], &[&gone.to_string()]))
            .is_ok()
    );
    let phase = |run| runtime.inspect_agent(run).unwrap().phase;
    let before = phase(gone);

    // Binding a root.
    assert!(runtime.bind_thread_root("gone-thread", kept).is_err());
    assert!(runtime.bind_thread_root("other-thread", gone).is_err());
    // Restoring a run.
    assert_deleted(runtime.resume_chat(gone, RunConfig::default()));
    assert_deleted(runtime.wake_chat(gone, RunConfig::default()));
    assert_deleted(runtime.continue_goal(gone, "again".into(), RunConfig::default()));
    // Starting a chat on the thread, plain or seeded from a deleted run.
    assert_deleted(runtime.delegate_chat(
        "gone-thread",
        Role::Worker,
        "hello".into(),
        RunConfig::default(),
    ));
    assert_deleted(runtime.delegate_chat_seeded(
        "fresh-thread",
        Role::Worker,
        "hello".into(),
        RunConfig::default(),
        Some(crate::ChatForkSeed {
            source_run_id: gone.to_string(),
            context_len: 2,
        }),
    ));
    // Host input to the deleted run.
    assert_deleted(runtime.send_agent_message(
        kept,
        gone,
        event_bus::AgentMessageKind::Send,
        "hi",
        None,
    ));
    // An escalation whose source is deleted.
    let target = runtime.shared.run_ids.next();
    let refused = crate::restore::persist_escalation_seed(
        runtime,
        target,
        gone,
        "memo",
        &RunConfig::default(),
        false,
        &Default::default(),
        None,
    );
    assert!(matches!(
        refused,
        Err(crate::restore::SnapshotError::ThreadDeleted(_))
    ));

    assert_eq!(phase(gone), before, "no refusal changed the run");
    assert!(
        fixture
            .runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .restore_record(target)
            .unwrap()
            .is_none(),
        "no snapshot was written for the refused escalation"
    );
    // A surviving run still works through the same entries.
    runtime.bind_thread_root("kept-thread", kept).unwrap();
    runtime.resume_chat(kept, RunConfig::default()).unwrap();
}
