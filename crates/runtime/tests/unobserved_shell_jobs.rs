//! A model that ends its turn while shell jobs are unobserved is reminded of them, by
//! name, a few times and then stopped: asking for ever with the same words would spend
//! a request each time without end.

mod support;

use std::sync::Arc;

use event_bus::{AgentRunPhase, Event, EventBus, EventKind, LifecycleEvent};
use providers::{ContentBlock, FinishReason, Role as MessageRole};
use runtime::{AgentRuntime, Role, RunConfig, RunStore};
use sandbox::DirectSandbox;
use serde_json::json;
use storage::{Storage, StorageConfig};
use support::{ScriptedModel, drain_events, text_response, tool_response};
use tools::ToolExecutor;

fn reason_of_error(events: &[Event]) -> Option<String> {
    events.iter().find_map(|event| match &event.kind {
        EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            to: AgentRunPhase::Error,
            reason: Some(reason),
            ..
        }) => Some(reason.clone()),
        _ => None,
    })
}

#[tokio::test]
async fn a_model_that_ignores_the_reminder_is_stopped_after_a_few_with_the_jobs_named() {
    let bus = Arc::new(EventBus::new(256));
    let mut events = bus.subscribe();
    let executor = ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    );
    // A background job whose result the model never reads, then the same report again
    // and again.
    let mut script = vec![Ok(tool_response(
        "start",
        "shell",
        json!({"command": "sleep 600", "yield_ms": 0}),
    ))];
    script.extend((0..10).map(|_| Ok(text_response("All done.", FinishReason::Stop))));
    let model = Arc::new(ScriptedModel::new(script));
    // With a durable store the run can record its unobserved effects, so its own reason
    // for stopping is the one reported.
    let directory = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: directory.path().join("run.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let runtime = AgentRuntime::new(Arc::clone(&bus), Arc::new(executor), model.clone())
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());

    let run = runtime.delegate_background(Role::Worker, "task".into(), RunConfig::default());
    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Error));
    let events = drain_events(&mut events).await;

    let reason = reason_of_error(&events).expect("the run ended with an error");
    assert!(
        reason.contains("shell jobs stayed unobserved after 3 reminders"),
        "{reason}"
    );
    assert!(reason.contains("job-"), "the jobs are named: {reason}");
    // The start, the first report and three more after a reminder each: no endless loop.
    let observed = model.observed().await;
    assert_eq!(observed.len(), 5, "{} requests", observed.len());
    let reminders: Vec<String> = observed
        .last()
        .unwrap()
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::Text { text } if text.starts_with("Shell jobs are still running") => {
                Some(text.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(reminders.len(), 3, "{reminders:?}");
    for reminder in &reminders {
        assert!(
            reminder.contains("job-") && reminder.contains("running"),
            "the job and its state are named: {reminder}"
        );
        assert!(reminder.contains("action:poll"), "{reminder}");
    }
}
