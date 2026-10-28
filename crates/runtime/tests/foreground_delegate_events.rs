//! Awaited delegates report one start and one completion per call.

mod support;

use std::sync::Arc;

use event_bus::{AgentRunPhase, Event, EventBus, EventKind, LifecycleEvent, ToolEvent};
use providers::FinishReason;
use runtime::{AgentRuntime, Role, RunConfig};
use sandbox::DirectSandbox;
use serde_json::json;
use storage::{Database, Storage, StorageConfig};
use tools::ToolExecutor;

use support::{ScriptedModel, text_response, tool_responses};

/// Runs `calls` as one parent batch and returns the parent's events up to its end.
async fn run_batch(
    calls: Vec<(&'static str, &'static str, serde_json::Value)>,
) -> (String, Vec<Event>) {
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "parent",
            [
                Ok(tool_responses(calls)),
                Ok(text_response("parent done", FinishReason::Stop)),
            ],
        )
        .await;
    for task in ["task a", "task b"] {
        model
            .add_keyed(task, [Ok(text_response("child done", FinishReason::Stop))])
            .await;
    }
    let bus = Arc::new(EventBus::new(512));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(Arc::clone(&bus), executor, model);
    let mut receiver = bus.subscribe();
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    let parent_id = parent.to_string();
    let mut events = Vec::new();
    loop {
        let event = receiver.recv().await.expect("bus open");
        let done = matches!(&event.kind, EventKind::Lifecycle(
            LifecycleEvent::AgentRunStateChanged { run_id, to: AgentRunPhase::Done, .. }
        ) if run_id == &parent_id);
        events.push(event);
        if done {
            break;
        }
    }
    (parent_id, events)
}

/// `(kind, call_id, is_error)` of the parent's delegate lifecycle events.
fn delegate_lifecycle(parent: &str, events: &[Event]) -> Vec<(&'static str, String, bool)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Tool(ToolEvent::ToolStarted {
                tool_name,
                call_id,
                run_id,
                ..
            }) if tool_name == "delegate" && run_id.as_deref() == Some(parent) => {
                Some(("start", call_id.clone(), false))
            }
            EventKind::Tool(ToolEvent::ToolCompleted {
                tool_name,
                call_id,
                run_id,
                is_error,
                ..
            }) if tool_name == "delegate" && run_id.as_deref() == Some(parent) => {
                Some(("end", call_id.clone(), *is_error))
            }
            _ => None,
        })
        .collect()
}

fn child(name: &'static str, prompt: &'static str) -> serde_json::Value {
    json!({"target": {"role": "worker"}, "name": name, "prompt": prompt, "background": false})
}

#[tokio::test]
async fn a_single_foreground_delegate_reports_start_and_completion() {
    let (parent, events) =
        run_batch(vec![("call-a", "delegate", child("child-a", "task a"))]).await;
    assert_eq!(
        delegate_lifecycle(&parent, &events),
        [
            ("start", "call-a".into(), false),
            ("end", "call-a".into(), false)
        ]
    );
}

#[tokio::test]
async fn parallel_foreground_delegates_report_each_call_and_failures() {
    // Given: two good delegates and one whose arguments cannot spawn a child.
    let (parent, events) = run_batch(vec![
        ("call-a", "delegate", child("child-a", "task a")),
        ("call-bad", "delegate", json!({"background": false})),
        ("call-b", "delegate", child("child-b", "task b")),
    ])
    .await;

    // Then: every call starts before the wave is awaited and completes on its own.
    let lifecycle = delegate_lifecycle(&parent, &events);
    let ended: Vec<_> = lifecycle
        .iter()
        .filter(|(kind, ..)| *kind == "end")
        .map(|(_, id, error)| (id.as_str(), *error))
        .collect();
    assert_eq!(
        ended,
        [("call-a", false), ("call-bad", true), ("call-b", false)]
    );
    let first_end = lifecycle
        .iter()
        .position(|(kind, ..)| *kind == "end")
        .unwrap();
    assert_eq!(
        first_end, 3,
        "all starts precede the first completion: {lifecycle:?}"
    );
    for id in ["call-a", "call-bad", "call-b"] {
        assert_eq!(
            lifecycle
                .iter()
                .filter(|(kind, call, _)| *kind == "start" && call == id)
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn foreground_delegate_events_are_persisted_for_restore() {
    // Given: a finished run whose events were appended to the store.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let (parent, events) =
        run_batch(vec![("call-a", "delegate", child("child-a", "task a"))]).await;
    for event in &events {
        storage
            .handle()
            .append_event(Some("delegate-restore"), event)
            .unwrap();
    }
    storage.close();

    // When: the history is read back from the store.
    let restored: Vec<_> = Database::open(&config)
        .unwrap()
        .events_all_ordered()
        .unwrap()
        .into_iter()
        .map(|row| row.event)
        .collect();

    // Then: the restored events still carry the delegate call's lifecycle.
    assert_eq!(
        delegate_lifecycle(&parent, &restored),
        delegate_lifecycle(&parent, &events)
    );
    assert_eq!(delegate_lifecycle(&parent, &restored).len(), 2);
}
