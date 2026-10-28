//! A model that ends its turn while shell jobs are unobserved is reminded of them, by
//! name, a few times and then stopped: asking for ever with the same words would spend
//! a request each time without end.

mod support;

use std::sync::Arc;

use event_bus::{AgentRunPhase, Event, EventBus, EventKind, LifecycleEvent};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolSpec};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RunStore, RuntimeError,
};
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

/// A runtime with a durable store over `model` (the run can record unobserved effects, so
/// its own reason for stopping is the one reported).
fn runtime_with_store(
    model: Arc<dyn AgentModel>,
    directory: &std::path::Path,
) -> (AgentRuntime, Arc<EventBus>, Storage) {
    let bus = Arc::new(EventBus::new(256));
    let executor = ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    );
    let config = StorageConfig {
        db_path: directory.join("run.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let runtime = AgentRuntime::new(Arc::clone(&bus), Arc::new(executor), model)
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    (runtime, bus, storage)
}

#[tokio::test]
async fn calls_that_have_nothing_to_do_with_the_jobs_do_not_restart_the_count() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("absent.txt");
    // A background job nobody reads, and between the reports a read of a file that is not
    // there, a call with arguments that are not valid, and an unrelated command: none of
    // them deals with the job.
    let mut script = vec![Ok(tool_response(
        "start",
        "shell",
        json!({"command": "sleep 600", "yield_ms": 0}),
    ))];
    for (index, call) in [
        ("failed-read", ("read", json!({"path": missing}))),
        ("invalid", ("read", json!({}))),
        ("unrelated", ("shell", json!({"command": "true"}))),
    ] {
        script.push(Ok(text_response("All done.", FinishReason::Stop)));
        script.push(Ok(tool_response(index, call.0, call.1)));
    }
    script.extend((0..5).map(|_| Ok(text_response("All done.", FinishReason::Stop))));
    let model = Arc::new(ScriptedModel::new(script));
    let (runtime, bus, _storage) = runtime_with_store(model.clone(), directory.path());
    let mut events = bus.subscribe();

    let run = runtime.delegate_background(Role::Worker, "task".into(), RunConfig::default());
    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Error));

    let reason = reason_of_error(&drain_events(&mut events).await).unwrap();
    assert!(
        reason.contains("shell jobs stayed unobserved after 3 reminders"),
        "{reason}"
    );
    // start, three reports each followed by an unrelated call, and the fourth report that
    // is the fourth reminder in a row.
    assert_eq!(model.observed().await.len(), 8);
}

/// Starts a background job, reports done, and then deals with the job (stops it and polls
/// the result) after `reminders_before` reminders; whatever the job is called is taken from the history.
struct DealsWithTheJob {
    step: std::sync::atomic::AtomicUsize,
    reminders_before: usize,
}

#[async_trait::async_trait]
impl AgentModel for DealsWithTheJob {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let step = self.step.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let handle = || {
            messages
                .iter()
                .flat_map(|message| message.content.iter())
                .find_map(|block| match block {
                    ContentBlock::ToolResult { content, .. } => content.iter().find_map(|part| {
                        let providers::ToolResultContent::Text { text } = part else {
                            return None;
                        };
                        let at = text.find("job-")?;
                        let digits: String = text[at + 4..]
                            .chars()
                            .take_while(char::is_ascii_digit)
                            .collect();
                        Some(format!("job-{digits}"))
                    }),
                    _ => None,
                })
        };
        Ok(if step == 0 {
            tool_response(
                "start",
                "shell",
                json!({"command": "sleep 600", "yield_ms": 0}),
            )
        } else if step <= self.reminders_before {
            text_response("All done.", FinishReason::Stop)
        } else if step == self.reminders_before + 1 {
            // Stopped, then polled to its terminal result: that observes the outcome.
            tool_response(
                "stop",
                "shell",
                json!({"action": "stop", "job_id": handle().expect("the job was named"), "yield_ms": 1000}),
            )
        } else if step == self.reminders_before + 2 {
            tool_response(
                "poll",
                "shell",
                json!({"action": "poll", "job_id": handle().expect("the job was named")}),
            )
        } else {
            text_response("All done, the job was stopped.", FinishReason::Stop)
        })
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "reactive".into()
    }
}

#[tokio::test]
async fn dealing_with_the_job_ends_the_reminders_and_the_run_finishes() {
    let directory = tempfile::tempdir().unwrap();
    // Three reminders are ignored, then the job is stopped and polled: that is within the limit and
    // resolves what the reminders asked for.
    let model = Arc::new(DealsWithTheJob {
        step: std::sync::atomic::AtomicUsize::new(0),
        reminders_before: 3,
    });
    let (runtime, bus, _storage) = runtime_with_store(model, directory.path());
    let mut events = bus.subscribe();

    let run = runtime.delegate_background(Role::Worker, "task".into(), RunConfig::default());
    let phase = runtime.wait(run).await;

    let events = drain_events(&mut events).await;
    assert_eq!(
        phase,
        Ok(AgentRunPhase::Done),
        "{:?}",
        reason_of_error(&events)
    );
}
