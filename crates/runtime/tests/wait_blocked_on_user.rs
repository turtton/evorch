//! `AgentRuntime::wait_or_blocked_on_user`: a host with no one to answer learns that the
//! run waits for the user from the run's state, not from events a slow subscriber can
//! lose.

mod support;

use std::sync::Arc;

use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent, RecvError};
use providers::FinishReason;
use runtime::{AgentRuntime, Role, RunConfig, RunStore, RunWait, StopScope};
use serde_json::json;
use storage::{Storage, StorageConfig};
use support::{ScriptedModel, text_response, tool_response};
use tools::ToolExecutor;

/// A runtime over a bus of `capacity` events whose model asks one question and then ends
/// its turn.
fn asking_runtime(
    directory: &std::path::Path,
    capacity: usize,
    blocking: bool,
) -> (AgentRuntime, Arc<EventBus>, Storage) {
    let config = StorageConfig {
        db_path: directory.join("questions.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(capacity));
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "ask",
            "ask_user",
            json!({"title": "Which one?", "blocking": blocking}),
        )),
        Ok(text_response("Which one?", FinishReason::Stop)),
    ]));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus.clone())), model)
        .with_sequential_run_ids()
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    // The storage must outlive the run: its writer closes with it.
    (runtime, bus, storage)
}

#[tokio::test]
async fn a_run_blocked_on_a_question_is_found_although_every_event_was_dropped() {
    let directory = tempfile::tempdir().unwrap();
    // A bus of two events and a subscriber that never reads: nearly everything is lost.
    let (runtime, bus, _storage) = asking_runtime(directory.path(), 2, true);
    let mut subscriber = bus.subscribe();
    let run = runtime.delegate_background(Role::Worker, "work".into(), RunConfig::default());

    let waited = runtime.wait_or_blocked_on_user(run).await.unwrap();

    let RunWait::BlockedOnUser(questions) = waited else {
        panic!("expected a blocked run, got {waited:?}");
    };
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].title, "Which one?");
    assert_eq!(
        runtime.inspect_agent(run).unwrap().phase,
        AgentRunPhase::Waiting
    );
    // The events that announced it were lost, which is the case this guards against.
    assert!(matches!(subscriber.recv().await, Err(RecvError::Lagged(_))));
    // A caller that starts late sees the same state.
    assert!(matches!(
        runtime.wait_or_blocked_on_user(run).await.unwrap(),
        RunWait::BlockedOnUser(_)
    ));
}

#[tokio::test]
async fn a_question_that_does_not_block_does_not_hold_the_wait() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, _bus, _storage) = asking_runtime(directory.path(), 256, false);
    let run = runtime.delegate_background(Role::Worker, "work".into(), RunConfig::default());

    let waited = runtime.wait_or_blocked_on_user(run).await.unwrap();

    assert_eq!(waited, RunWait::Ended(AgentRunPhase::Done));
}

#[tokio::test]
async fn waiting_for_the_next_user_turn_without_a_question_is_not_blocked_on_the_user() {
    let bus = Arc::new(EventBus::new(256));
    let mut events = bus.subscribe();
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "Ready.",
        FinishReason::Stop,
    ))]));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model)
        .with_sequential_run_ids();
    let run = runtime.delegate_background(
        Role::Worker,
        "work".into(),
        RunConfig {
            interactive: true,
            ..RunConfig::default()
        },
    );
    // The run answers and then waits for the user's next message: no question exists.
    loop {
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            to: AgentRunPhase::Waiting,
            ..
        }) = events.recv().await.unwrap().kind
        {
            break;
        }
    }
    let mut wait = Box::pin(runtime.wait_or_blocked_on_user(run));

    assert!(
        futures_util::poll!(wait.as_mut()).is_pending(),
        "a wait for input without a question is not a blocked run"
    );
    runtime.stop(run, StopScope::SelfOnly).unwrap();
    assert!(matches!(wait.await.unwrap(), RunWait::Ended(_)));
}
