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

// Given: a guard listing a deleted thread and run
// When: the host restores goal or todo state, creates a goal, declares read-only
// directories, answers a question of the deleted run, or compacts it / Then: every one
// is refused before any state, event or answer changes, and the same calls work for a
// surviving thread.
#[tokio::test]
async fn state_restores_goals_and_answers_cannot_revive_a_deleted_thread() {
    let fixture = Fixture::new();
    let runtime = &fixture.runtime;
    let gone = fixture.terminal().await;
    let kept = fixture.terminal().await;
    runtime.bind_thread_root("kept-thread", kept).unwrap();
    let goal = runtime
        .create_thread_goal("kept-thread", kept, "objective".into(), vec!["done".into()])
        .unwrap();
    let todo = event_bus::ThreadTodoSnapshot {
        list_id: "list".into(),
        thread_id: "kept-thread".into(),
        revision: 1,
        items: vec![event_bus::ThreadTodoItem {
            content: "item".into(),
            status: event_bus::ThreadTodoStatus::Pending,
        }],
    };
    let question = event_bus::UserQuestion {
        responder: None,
        routing_revision: 0,
        management_revision: None,
        id: "question-of-gone".into(),
        run_id: gone.to_string(),
        root_run_id: gone.to_string(),
        root_name: "gone".into(),
        recipient_run_ids: vec![],
        title: "?".into(),
        options: vec![],
        blocking: false,
        answer: None,
        answer_origin: None,
    };
    fixture
        .storage
        .handle()
        .create_user_question(&question)
        .unwrap();
    runtime
        .set_tombstone_guard(tombstones(&["gone-thread"], &[&gone.to_string()]))
        .unwrap();

    // Goals.
    let mut snapshot = goal.clone();
    snapshot.thread_id = "gone-thread".into();
    assert!(runtime.restore_thread_goal(snapshot.clone()).is_err());
    let mut snapshot = goal.clone();
    snapshot.root_run_id = gone.to_string();
    assert!(runtime.restore_thread_goal(snapshot).is_err());
    let mut snapshot = goal.clone();
    snapshot.related_root_run_ids = vec![gone.to_string()];
    assert!(runtime.restore_thread_goal(snapshot).is_err());
    assert!(
        runtime
            .create_thread_goal("gone-thread", kept, "x".into(), vec!["y".into()])
            .is_err()
    );
    assert!(runtime.thread_goal("gone-thread").is_none());
    // Todos.
    let mut deleted_todo = todo.clone();
    deleted_todo.thread_id = "gone-thread".into();
    assert!(runtime.restore_thread_todo(deleted_todo).is_err());
    assert!(runtime.thread_todo("gone-thread").is_none());
    // Declared directories are not kept for it.
    let dir = tempfile::tempdir().unwrap();
    runtime.set_thread_read_only_directories("gone-thread", vec![dir.path().to_path_buf()]);
    assert!(
        runtime
            .thread_read_only_directories("gone-thread")
            .is_empty()
    );
    // Answers and compaction of the deleted run.
    assert!(
        runtime
            .answer_user_question("question-of-gone", "yes")
            .is_err()
    );
    assert!(
        runtime
            .user_question("question-of-gone")
            .unwrap()
            .unwrap()
            .answer
            .is_none()
    );
    assert_deleted(
        runtime
            .compact_saved_chat(gone, RunConfig::default())
            .await
            .map_err(|error| match error {
                RuntimeError::RunRestoreFailed { .. } => error,
                other => panic!("{other:?}"),
            }),
    );

    // The same entries work for the surviving thread.
    runtime.restore_thread_goal(goal).unwrap();
    runtime.restore_thread_todo(todo).unwrap();
    runtime.set_thread_read_only_directories("kept-thread", vec![dir.path().to_path_buf()]);
    assert_eq!(runtime.thread_read_only_directories("kept-thread").len(), 1);
}
