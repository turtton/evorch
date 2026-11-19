use std::sync::Arc;

use agents::Role;
use event_bus::{AgentMessageKind, EventBus};
use futures_util::poll;
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec, Usage};
use tokio::sync::Notify;
use tools::ToolExecutor;

use super::*;
use crate::{AgentInvocationContext, AgentModel, RunConfig, RuntimeError};

#[path = "lane_tests.rs"]
mod lane_tests;

struct GatedModel {
    first: Notify,
    second: Notify,
    admission: Option<Arc<Notify>>,
}

#[async_trait::async_trait]
impl AgentModel for GatedModel {
    fn requires_admission(&self) -> bool {
        self.admission.is_some()
    }

    async fn admit(&self, _: &AgentInvocationContext, role: Role) -> Result<(), RuntimeError> {
        if let Some(gate) = &self.admission {
            gate.notified().await;
        }
        if role == Role::Reviewer {
            return Err(RuntimeError::Model {
                reason: "admission rejected".into(),
            });
        }
        Ok(())
    }

    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let prompt = messages
            .iter()
            .find(|message| message.role == providers::Role::User)
            .and_then(|message| message.content.first());
        match prompt {
            Some(ContentBlock::Text { text }) if text == "first" => self.first.notified().await,
            Some(ContentBlock::Text { text }) if text == "second" => self.second.notified().await,
            Some(ContentBlock::Text { text }) if text == "failure" => {
                self.first.notified().await;
                return Err(RuntimeError::Model {
                    reason: "失敗".repeat(2000),
                });
            }
            _ => std::future::pending::<()>().await,
        }
        Ok(ChatResponse {
            usage_reported: true,
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "検証済み".repeat(2000),
                }],
            },
            usage: Usage::default(),
            finish_reason: FinishReason::Stop,
        })
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "gated".into()
    }
}

fn fixture(admission: Option<Arc<Notify>>) -> (AgentRuntime, Arc<GatedModel>) {
    let model = Arc::new(GatedModel {
        first: Notify::new(),
        second: Notify::new(),
        admission,
    });
    let bus = Arc::new(EventBus::new(128));
    (
        AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone()),
        model,
    )
}

fn spawn(runtime: &AgentRuntime) -> (RunId, RunId, RunId) {
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    let first = runtime
        .delegate_background_as_child(parent, Role::Worker, "first", RunConfig::default())
        .unwrap();
    let second = runtime
        .delegate_background_as_child(parent, Role::Worker, "second", RunConfig::default())
        .unwrap();
    (parent, first, second)
}

/// A request with an internal deadline, so tests can also observe a wait that
/// ends without an event. Model waits have no deadline.
fn request(runs: Vec<RunId>, mode: WaitMode, timeout_ms: u64) -> WaitRequest {
    WaitRequest {
        runs,
        mode,
        observed: HashSet::new(),
        shown_shell: HashSet::new(),
        executor: None,
        delegate: false,
        deadline: Some(Duration::from_millis(timeout_ms)),
        legacy: false,
        reported: HashSet::new(),
        delivered: HashSet::new(),
    }
}

const LONG_MS: u64 = 600_000;

async fn cleanup(runtime: &AgentRuntime) {
    for run in runtime.list_agents() {
        if !matches!(run.phase, AgentRunPhase::Done | AgentRunPhase::Error) {
            runtime.cancel(run.run_id).unwrap();
            runtime.wait(run.run_id).await.unwrap();
        }
    }
}

#[test]
fn wait_schema_and_argument_validation_agree() {
    let validator =
        jsonschema::validator_for(&crate::meta::tool_spec("wait").input_schema).unwrap();
    for input in [
        json!({}),
        json!({"run_id":"run-1"}),
        json!({"run_ids":["run-1","run-2"],"mode":"all"}),
        json!({"run_ids":["run-1"],"mode":"any"}),
    ] {
        assert!(validator.is_valid(&input));
        assert!(parse::<WaitArgs>(input).unwrap().validate().is_ok());
    }
    for input in [
        json!({"mode":"all"}),
        json!({"run_id":"run-1","run_ids":["run-2"]}),
        json!({"run_ids":[]}),
        json!({"run_ids":["run-1","run-1"]}),
        // A wait has no deadline; the agent loop drops a legacy timeout_ms
        // before validation, so it never reaches the schema or the handler.
        json!({"run_id":"run-1","timeout_ms":600000}),
        json!({"run_id":"run-1","mode":"some"}),
        json!({"run_id":"run-1","poll_interval":1}),
        json!({"run_ids":["run-1","run-2","run-3","run-4","run-5","run-6","run-7","run-8","run-9"]}),
    ] {
        assert!(!validator.is_valid(&input), "schema accepted {input}");
        assert!(
            parse::<WaitArgs>(input.clone())
                .and_then(WaitArgs::validate)
                .is_err(),
            "handler accepted {input}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn any_wakes_on_one_completion_and_returns_bounded_utf8_output() {
    let (runtime, model) = fixture(None);
    let (parent, first, second) = spawn(&runtime);
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let request = request(vec![first, second], WaitMode::Any, 60000);
    let wait = observe(&runtime, parent, &request, receiver);
    tokio::pin!(wait);
    assert!(poll!(&mut wait).is_pending());
    model.first.notify_one();
    runtime.wait(first).await.unwrap();
    let result = wait.await.unwrap();
    assert_eq!(result["timed_out"], false);
    assert_eq!(result["completed_run_ids"], json!([first.to_string()]));
    assert_eq!(result["runs"].as_array().unwrap().len(), 1);
    assert_eq!(result["runs"][0]["output_truncated"], true);
    let original = serde_json::to_value(runtime.run_output(parent, first).unwrap()).unwrap();
    assert_eq!(result["runs"][0]["output_ref"], original["output_ref"]);
    assert_eq!(
        result["runs"][0]["output_bytes"],
        original["output"].as_str().unwrap().len()
    );
    assert!(result["runs"][0]["output"].as_str().unwrap().len() <= OUTPUT_BYTES);
    // Observing completion must not consume the parent's completion relay.
    assert!(!runtime.take_inbox(parent).unwrap().is_empty());
    cleanup(&runtime).await;
}

#[tokio::test(start_paused = true)]
async fn all_waits_for_every_completion_including_cancelled_children() {
    let (runtime, model) = fixture(None);
    let (parent, first, second) = spawn(&runtime);
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let request = request(vec![first, second], WaitMode::All, 60000);
    let wait = observe(&runtime, parent, &request, receiver);
    tokio::pin!(wait);
    assert!(poll!(&mut wait).is_pending());
    model.first.notify_one();
    runtime.wait(first).await.unwrap();
    // A completion relay is still queued but must not interrupt mode=all.
    assert!(poll!(&mut wait).is_pending());
    runtime.cancel(second).unwrap();
    runtime.wait(second).await.unwrap();
    let result = wait.await.unwrap();
    assert_eq!(result["timed_out"], false);
    assert_eq!(result["runs"][1]["status"], "cancelled");
    assert_eq!(
        result["completed_run_ids"],
        json!([first.to_string(), second.to_string()])
    );
    cleanup(&runtime).await;
}

#[tokio::test(start_paused = true)]
async fn timeout_returns_snapshot_without_cancelling_work() {
    let (runtime, _) = fixture(None);
    let (parent, first, second) = spawn(&runtime);
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let began = tokio::time::Instant::now();
    let result = observe(
        &runtime,
        parent,
        &request(vec![first, second], WaitMode::All, 5000),
        receiver,
    )
    .await
    .unwrap();
    assert_eq!(began.elapsed(), Duration::from_millis(5000));
    assert_eq!(result["timed_out"], true);
    assert!(
        result["runs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|run| run["status"] == "still_running")
    );
    cleanup(&runtime).await;
}

#[tokio::test(start_paused = true)]
async fn zero_timeout_is_immediate_and_already_completed_does_not_time_out() {
    let (runtime, model) = fixture(None);
    let (parent, first, _) = spawn(&runtime);
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let began = tokio::time::Instant::now();
    assert_eq!(
        observe(
            &runtime,
            parent,
            &request(vec![first], WaitMode::Any, 0),
            receiver.clone()
        )
        .await
        .unwrap()["timed_out"],
        true
    );
    model.first.notify_one();
    runtime.wait(first).await.unwrap();
    assert_eq!(
        observe(
            &runtime,
            parent,
            &request(vec![first], WaitMode::Any, 0),
            receiver
        )
        .await
        .unwrap()["timed_out"],
        false
    );
    assert_eq!(began.elapsed(), Duration::ZERO);
    cleanup(&runtime).await;
}

#[tokio::test(start_paused = true)]
async fn cancellation_wins_over_simultaneous_completion() {
    let (runtime, model) = fixture(None);
    let (parent, first, _) = spawn(&runtime);
    let (cancel, receiver) = watch::channel(RunInterrupt::None);
    let request = request(vec![first], WaitMode::Any, 60000);
    let wait = observe(&runtime, parent, &request, receiver.clone());
    tokio::pin!(wait);
    assert!(poll!(&mut wait).is_pending());
    model.first.notify_one();
    runtime.wait(first).await.unwrap();
    cancel.send_replace(RunInterrupt::Cancel);
    assert_eq!(wait.await.unwrap_err(), "wait cancelled");
    assert_eq!(
        observe(&runtime, parent, &request, receiver)
            .await
            .unwrap_err(),
        "wait cancelled"
    );
    cleanup(&runtime).await;
}

#[tokio::test]
async fn self_siblings_unrelated_and_unknown_runs_are_denied() {
    let (runtime, _) = fixture(None);
    let (parent, first, second) = spawn(&runtime);
    let unrelated =
        runtime.delegate_background(Role::Worker, "unrelated".into(), RunConfig::default());
    for (caller, target) in [
        (parent, parent),
        (first, second),
        (parent, unrelated),
        (parent, RunId::new(999)),
    ] {
        let (_cancel, receiver) = watch::channel(RunInterrupt::None);
        assert!(
            observe(
                &runtime,
                caller,
                &request(vec![target], WaitMode::Any, 0),
                receiver
            )
            .await
            .is_err()
        );
    }
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    assert!(
        observe(
            &runtime,
            first,
            &request(vec![parent], WaitMode::Any, 0),
            receiver
        )
        .await
        .is_ok()
    );
    cleanup(&runtime).await;
}

#[tokio::test(start_paused = true)]
async fn child_admission_can_be_observed_before_registration_without_weakening_access() {
    let admission = Arc::new(Notify::new());
    let (runtime, model) = fixture(Some(admission.clone()));
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    admission.notify_one();
    runtime.wait_admission(parent).await.unwrap();
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "first", RunConfig::default())
        .unwrap();
    let unrelated =
        runtime.delegate_background(Role::Worker, "unrelated".into(), RunConfig::default());
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let pending = observe(
        &runtime,
        parent,
        &request(vec![child], WaitMode::Any, 0),
        receiver.clone(),
    )
    .await
    .unwrap();
    assert_eq!(pending["runs"], json!([]));
    assert_eq!(
        serde_json::to_value(runtime.run_output(parent, child).unwrap()).unwrap()["phase"],
        "Pending"
    );
    assert!(
        observe(
            &runtime,
            parent,
            &request(vec![unrelated], WaitMode::Any, 0),
            receiver.clone()
        )
        .await
        .is_err()
    );
    let request = request(vec![child], WaitMode::Any, 60000);
    let wait = observe(&runtime, parent, &request, receiver);
    tokio::pin!(wait);
    assert!(poll!(&mut wait).is_pending());
    // Both queued admissions subscribe before this broadcast; pin/poll their
    // admission futures through a scheduler yield instead of wall-clock sleeps.
    tokio::task::yield_now().await;
    admission.notify_waiters();
    model.first.notify_one();
    let result = wait.await.unwrap();
    assert_eq!(result["timed_out"], false);
    assert_eq!(result["runs"][0]["status"], "completed");
    cleanup(&runtime).await;
}

#[tokio::test(start_paused = true)]
async fn model_failure_is_a_terminal_result_with_bounded_reason() {
    let (runtime, model) = fixture(None);
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "failure", RunConfig::default())
        .unwrap();
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let request = request(vec![child], WaitMode::All, 60000);
    let wait = observe(&runtime, parent, &request, receiver);
    tokio::pin!(wait);
    assert!(poll!(&mut wait).is_pending());
    model.first.notify_one();
    let result = wait.await.unwrap();
    assert_eq!(result["timed_out"], false);
    assert_eq!(result["runs"][0]["status"], "failed");
    assert_eq!(result["runs"][0]["reason_truncated"], true);
    assert!(result["runs"][0]["reason"].as_str().unwrap().len() <= REASON_BYTES);
    cleanup(&runtime).await;
}

#[tokio::test(start_paused = true)]
async fn admission_failure_is_observable_without_a_registered_run() {
    let admission = Arc::new(Notify::new());
    let (runtime, _) = fixture(Some(admission.clone()));
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    admission.notify_one();
    runtime.wait_admission(parent).await.unwrap();
    let child = runtime
        .delegate_background_as_child(parent, Role::Reviewer, "failure", RunConfig::default())
        .unwrap();
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let request = request(vec![child], WaitMode::All, 60000);
    let wait = observe(&runtime, parent, &request, receiver.clone());
    tokio::pin!(wait);
    assert!(poll!(&mut wait).is_pending());
    admission.notify_one();
    let result = wait.await.unwrap();
    assert_eq!(result["runs"][0]["status"], "failed");
    assert!(
        result["runs"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("admission rejected")
    );
    assert!(runtime.inspect_agent(child).is_err());
    assert_eq!(
        observe(&runtime, parent, &request, receiver).await.unwrap()["runs"][0]["status"],
        "failed"
    );
    cleanup(&runtime).await;
}

#[tokio::test(start_paused = true)]
async fn optional_child_question_wakes_parent_wait_for_orchestrator_resolution() {
    let directory = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: directory.path().join("questions.sqlite3"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let (runtime, _) = fixture(None);
    let runtime = runtime.with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
    let (parent, first, second) = spawn(&runtime);
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let request = request(vec![first, second], WaitMode::All, 60000);
    let wait = observe(&runtime, parent, &request, receiver.clone());
    tokio::pin!(wait);
    assert!(poll!(&mut wait).is_pending());
    let question = runtime
        .request_user_question(second, "Optional preference".into(), vec![], false)
        .unwrap();
    let result = wait.await.unwrap();
    assert_eq!(result["attention_run_ids"], json!([second.to_string()]));
    assert_eq!(result["runs"][0]["needs_user_input"], false);
    assert_eq!(result["runs"][0]["has_pending_question"], true);
    assert_eq!(
        runtime.subagent_questions(parent, second).unwrap(),
        vec![question.clone()]
    );
    runtime
        .answer_subagent_question(parent, &question.id, "Use A")
        .await
        .unwrap();
    let result = observe(
        &runtime,
        parent,
        &self::request(vec![second], WaitMode::Any, 0),
        receiver,
    )
    .await
    .unwrap();
    assert_eq!(result["attention_run_ids"], json!([]));
    cleanup(&runtime).await;
}

#[tokio::test(start_paused = true)]
async fn required_question_wakes_wait_without_consuming_or_copying_answers() {
    let directory = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: directory.path().join("questions.sqlite3"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let (runtime, _) = fixture(None);
    let runtime = runtime.with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
    let (parent, first, second) = spawn(&runtime);
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let request = request(vec![first, second], WaitMode::All, 60000);
    let began = tokio::time::Instant::now();
    let wait = observe(&runtime, parent, &request, receiver.clone());
    tokio::pin!(wait);
    assert!(poll!(&mut wait).is_pending());
    let question = runtime
        .request_user_question(
            first,
            "Required decision".into(),
            vec!["A".into(), "B".into()],
            true,
        )
        .unwrap();
    let result = wait.await.unwrap();
    assert_eq!(began.elapsed(), Duration::ZERO);
    assert_eq!(result["timed_out"], false);
    assert_eq!(result["attention_run_ids"], json!([first.to_string()]));
    assert_eq!(result["runs"][0]["needs_user_input"], true);
    assert!(!result.to_string().contains("Required decision"));
    // A later snapshot sees the still-pending question, even without a new event.
    assert_eq!(
        observe(&runtime, parent, &request, receiver.clone())
            .await
            .unwrap()["attention_run_ids"],
        json!([first.to_string()])
    );
    runtime.answer_user_question(&question.id, "A").unwrap();
    let result = observe(
        &runtime,
        parent,
        &self::request(vec![first], WaitMode::Any, 0),
        receiver,
    )
    .await
    .unwrap();
    assert_eq!(result["attention_run_ids"], json!([]));
    assert_eq!(result["timed_out"], true);
    assert_eq!(
        runtime.user_answers(first).unwrap()[0].answer.as_deref(),
        Some("A")
    );
    cleanup(&runtime).await;
}

#[tokio::test(start_paused = true)]
async fn model_wait_has_no_deadline_and_ends_on_completion() {
    let (runtime, model) = fixture(None);
    let (parent, first, _) = spawn(&runtime);
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let request = parse::<WaitArgs>(json!({"run_ids":[first.to_string()]}))
        .unwrap()
        .validate()
        .unwrap();
    assert!(request.deadline.is_none());
    let began = tokio::time::Instant::now();
    let wait = observe(&runtime, parent, &request, receiver);
    tokio::pin!(wait);
    assert!(poll!(&mut wait).is_pending());
    // Hours without an event keep waiting.
    tokio::time::advance(Duration::from_secs(5 * 60 * 60)).await;
    assert!(poll!(&mut wait).is_pending());
    model.first.notify_one();
    let result = wait.await.unwrap();
    assert_eq!(began.elapsed(), Duration::from_secs(5 * 60 * 60));
    assert!(result.get("timed_out").is_none(), "{result:?}");
    assert_eq!(result["completed_run_ids"], json!([first.to_string()]));
    cleanup(&runtime).await;
}

#[tokio::test(start_paused = true)]
async fn reported_questions_do_not_end_a_later_wait() {
    let directory = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: directory.path().join("questions.sqlite3"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let (runtime, model) = fixture(None);
    let runtime = runtime.with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
    let (parent, first, _) = spawn(&runtime);
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let question = runtime
        .request_user_question(first, "Optional preference".into(), vec![], false)
        .unwrap();
    let mut request = parse::<WaitArgs>(json!({"run_ids":[first.to_string()]}))
        .unwrap()
        .validate()
        .unwrap();
    // Unreported, the pending question ends the wait at once.
    let result = observe(&runtime, parent, &request, receiver.clone())
        .await
        .unwrap();
    assert_eq!(result["attention_run_ids"], json!([first.to_string()]));
    // Once reported, it no longer does; the wait lasts until the target finishes.
    request.reported.insert(question.id.clone());
    let wait = observe(&runtime, parent, &request, receiver.clone());
    tokio::pin!(wait);
    assert!(poll!(&mut wait).is_pending());
    tokio::time::advance(Duration::from_secs(60 * 60)).await;
    assert!(poll!(&mut wait).is_pending());
    // A second, new question ends it again.
    runtime
        .request_user_question(first, "Another choice".into(), vec![], false)
        .unwrap();
    let result = wait.await.unwrap();
    assert_eq!(result["attention_run_ids"], json!([first.to_string()]));
    assert_eq!(result["runs"][0]["has_pending_question"], true);
    model.first.notify_one();
    cleanup(&runtime).await;
}

#[tokio::test(start_paused = true)]
async fn message_interrupts_all_wait_without_consuming_or_copying_the_inbox() {
    let (runtime, _) = fixture(None);
    let (parent, first, second) = spawn(&runtime);
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let request = request(vec![first, second], WaitMode::All, LONG_MS);
    let began = tokio::time::Instant::now();
    let wait = observe(&runtime, parent, &request, receiver.clone());
    tokio::pin!(wait);
    assert!(poll!(&mut wait).is_pending());
    // Empty mailbox drains also publish version changes; they are not input.
    assert!(runtime.take_inbox(parent).unwrap().is_empty());
    assert!(poll!(&mut wait).is_pending());
    let message_id = runtime
        .send_agent_message(
            second,
            parent,
            AgentMessageKind::Send,
            "Need a decision",
            None,
        )
        .unwrap();
    let result = wait.await.unwrap();
    assert_eq!(began.elapsed(), Duration::ZERO);
    assert_eq!(result["timed_out"], false);
    assert_eq!(result["inbox_ready"], true);
    assert_eq!(result["completed_run_ids"], json!([]));
    assert!(!result.to_string().contains("Need a decision"));
    // Already queued input must also wake a later wait without a fresh event.
    assert_eq!(
        observe(&runtime, parent, &request, receiver.clone())
            .await
            .unwrap()["inbox_ready"],
        true
    );
    let messages = runtime.take_inbox(parent).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].message_id, message_id);
    assert_eq!(messages[0].content, "Need a decision");
    let result = observe(
        &runtime,
        parent,
        &self::request(vec![first, second], WaitMode::All, 0),
        receiver,
    )
    .await
    .unwrap();
    assert_eq!(result["inbox_ready"], false);
    assert_eq!(result["timed_out"], true);
    cleanup(&runtime).await;
}

#[tokio::test(start_paused = true)]
async fn messages_from_children_outside_wait_targets_interrupt_wait() {
    let (runtime, _) = fixture(None);
    let (parent, first, second) = spawn(&runtime);
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let request = request(vec![first], WaitMode::Any, LONG_MS);
    let wait = observe(&runtime, parent, &request, receiver);
    tokio::pin!(wait);
    assert!(poll!(&mut wait).is_pending());
    runtime
        .send_agent_message(second, parent, AgentMessageKind::Send, "Finding", None)
        .unwrap();
    let result = wait.await.unwrap();
    assert_eq!(result["inbox_ready"], true);
    assert_eq!(result["timed_out"], false);
    assert_eq!(result["runs"], json!([]));
    cleanup(&runtime).await;
}

#[tokio::test(start_paused = true)]
async fn cancellation_wins_over_a_simultaneous_inbox_message() {
    let (runtime, _) = fixture(None);
    let (parent, first, _) = spawn(&runtime);
    let (cancel, receiver) = watch::channel(RunInterrupt::None);
    let request = request(vec![first], WaitMode::Any, LONG_MS);
    let wait = observe(&runtime, parent, &request, receiver);
    tokio::pin!(wait);
    assert!(poll!(&mut wait).is_pending());
    runtime
        .send_agent_message(first, parent, AgentMessageKind::Send, "Finding", None)
        .unwrap();
    cancel.send_replace(RunInterrupt::Cancel);
    assert_eq!(wait.await.unwrap_err(), "wait cancelled");
    assert_eq!(runtime.take_inbox(parent).unwrap().len(), 1);
    cleanup(&runtime).await;
}

// A turn boundary is an explicit signal, not a wall-clock completion deadline.
async fn idle_turn(runtime: &AgentRuntime, model: &GatedModel, run: RunId, first: bool) {
    let mut events = runtime.shared.bus.subscribe();
    if first {
        model.first.notify_one();
    } else {
        model.second.notify_one();
    }
    loop {
        if let event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::TurnCompleted {
            run_id,
            ..
        }) = events.recv().await.unwrap().kind
            && run_id == run.to_string()
        {
            break;
        }
    }
}

#[tokio::test]
async fn interactive_turn_wakes_wait_with_output_before_run_terminates() {
    let (runtime, model) = fixture(None);
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    let child = runtime
        .delegate_background_as_child(
            parent,
            Role::Worker,
            "first",
            RunConfig {
                interactive: true,
                ..RunConfig::default()
            },
        )
        .unwrap();
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let mut request = request(vec![child], WaitMode::Any, 0);
    request.deadline = None;
    let wait = observe(&runtime, parent, &request, receiver);
    tokio::pin!(wait);
    assert!(poll!(&mut wait).is_pending());
    idle_turn(&runtime, &model, child, true).await;
    let result = poll!(&mut wait);
    assert!(
        result.is_ready(),
        "an interactive turn must wake wait without terminal state or external input"
    );
    let std::task::Poll::Ready(Ok(result)) = result else {
        unreachable!()
    };
    assert_eq!(result["completed_run_ids"], json!([child.to_string()]));
    assert_eq!(result["runs"][0]["phase"], "Waiting");
    assert_eq!(result["runs"][0]["status"], "turn_completed");
    assert!(
        result["runs"][0]["output"]
            .as_str()
            .unwrap()
            .starts_with("検証済み")
    );
    assert_eq!(result["runs"][0]["output_truncated"], true);
    assert_eq!(result["runs"][0]["has_pending_question"], false);
    cleanup(&runtime).await;
}

#[tokio::test]
async fn all_waits_for_interactive_turns_and_reports_each_generation_once() {
    let (runtime, model) = fixture(None);
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    let config = RunConfig {
        interactive: true,
        keep_alive: true,
        ..RunConfig::default()
    };
    let first = runtime
        .delegate_background_as_child(parent, Role::Worker, "first", config.clone())
        .unwrap();
    let second = runtime
        .delegate_background_as_child(parent, Role::Worker, "second", config)
        .unwrap();
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let mut request = request(vec![first, second], WaitMode::All, 0);
    request.deadline = None;
    let wait = observe(&runtime, parent, &request, receiver.clone());
    tokio::pin!(wait);
    assert!(poll!(&mut wait).is_pending());
    idle_turn(&runtime, &model, first, true).await;
    assert!(
        poll!(&mut wait).is_pending(),
        "mode=all must wait for both turns"
    );
    idle_turn(&runtime, &model, second, false).await;
    let result = poll!(&mut wait);
    assert!(
        result.is_ready(),
        "mode=all must accept idle interactive turns"
    );
    let std::task::Poll::Ready(Ok(result)) = result else {
        unreachable!()
    };
    assert_eq!(
        result["completed_run_ids"],
        json!([first.to_string(), second.to_string()])
    );
    for notice in &result.notices {
        runtime.observe_completion(notice);
    }
    let next = observe(&runtime, parent, &request, receiver);
    tokio::pin!(next);
    assert!(
        poll!(&mut next).is_pending(),
        "observed idle generations must not spin"
    );
    let mut events = runtime.shared.bus.subscribe();
    runtime
        .send_agent_message(
            parent,
            first,
            event_bus::AgentMessageKind::Send,
            "follow up",
            None,
        )
        .unwrap();
    loop {
        if let event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
            run_id,
            to: AgentRunPhase::Running,
            ..
        }) = events.recv().await.unwrap().kind
            && run_id == first.to_string()
        {
            break;
        }
    }
    let running = serde_json::to_value(runtime.run_output(parent, first).unwrap()).unwrap();
    assert_eq!(running["status"], "still_running");
    assert_eq!(
        running["output"],
        Value::Null,
        "a new turn must not expose stale output"
    );
    idle_turn(&runtime, &model, first, true).await;
    assert!(poll!(&mut next).is_pending());
    runtime
        .send_agent_message(
            parent,
            second,
            event_bus::AgentMessageKind::Send,
            "follow up",
            None,
        )
        .unwrap();
    idle_turn(&runtime, &model, second, false).await;
    // Observing a retained old snapshot must never acknowledge the new turn.
    for notice in &result.notices {
        runtime.observe_completion(notice);
    }
    let next = poll!(&mut next);
    assert!(
        next.is_ready(),
        "new turn generations must wake the next wait"
    );
    let std::task::Poll::Ready(Ok(next)) = next else {
        unreachable!()
    };
    assert_ne!(
        result["runs"][0]["turn_generation"],
        next["runs"][0]["turn_generation"]
    );
    cleanup(&runtime).await;
}

/// The queued receipt and the attention snapshot are one model-facing report,
/// including synchronous delegate waits that deliberately leave messages queued.
#[tokio::test]
async fn question_attention_consumes_notice_for_wait_and_synchronous_delegate() {
    for synchronous in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: directory.path().join("dedup.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let (_cancel, receiver) = watch::channel(RunInterrupt::None);
        let (runtime, mut state) =
            crate::agent_loop::delegate_cleanup_tests::started_fixture(receiver).await;
        let runtime =
            runtime.with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        let parent = state.caller_run_id();
        let child = runtime
            .delegate_background_as_child(
                parent,
                Role::Worker,
                "waiting child",
                RunConfig::default(),
            )
            .unwrap();
        let q = runtime
            .request_user_question(child, "One question".into(), vec![], false)
            .unwrap();
        state.transition(AgentRunPhase::Running, None).unwrap();
        let result = wait_with(
            &mut state,
            &runtime,
            json!({"run_ids":[child.to_string()]}),
            !synchronous,
        )
        .await;
        let value: Value = serde_json::from_str(&result.result.content).unwrap();
        assert!(
            !runtime.question_reported(&q.id),
            "peeking is not presentation"
        );
        state.context.push_tool_result("test-wait", result.result);
        result.observations.presented(&mut state);
        let result = value;
        assert_eq!(result["attention_question_ids"], json!([q.id.clone()]));
        assert!(state.reported_questions.contains(&q.id));
        assert!(runtime.question_reported(&q.id));
        assert!(
            runtime.take_inbox(parent).unwrap().is_empty(),
            "attention must consume the duplicate queued notice even for synchronous delegate"
        );
        cleanup(&runtime).await;
    }
}

#[tokio::test]
async fn observed_child_questions_do_not_repeat_attention_before_terminal_event() {
    for blocking in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: directory.path().join("observed-question.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let (runtime, _) = fixture(None);
        let runtime =
            runtime.with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        let (parent, child, _) = spawn(&runtime);
        let question = runtime
            .request_user_question(child, "One attention event".into(), vec![], blocking)
            .unwrap();
        assert_eq!(runtime.take_inbox(parent).unwrap().len(), 1);
        assert!(runtime.question_reported(&question.id));
        let (_cancel, receiver) = watch::channel(RunInterrupt::None);
        let request = parse::<WaitArgs>(json!({"run_ids":[child.to_string()]}))
            .unwrap()
            .validate()
            .unwrap();
        let waiting = observe(&runtime, parent, &request, receiver);
        tokio::pin!(waiting);
        assert!(poll!(&mut waiting).is_pending());
        runtime.cancel(child).unwrap();
        let result = waiting.await.unwrap();
        assert_eq!(result["attention_question_ids"], json!([]));
        assert_eq!(result["completed_run_ids"], json!([child.to_string()]));
        assert!(
            runtime
                .user_question(&question.id)
                .unwrap()
                .unwrap()
                .answer
                .is_none()
        );
        cleanup(&runtime).await;
    }
}

#[tokio::test]
async fn recording_a_wait_snapshot_does_not_consume_a_later_saved_question() {
    let directory = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: directory.path().join("snapshot.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) =
        crate::agent_loop::delegate_cleanup_tests::started_fixture(receiver.clone()).await;
    let runtime = runtime.with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
    let parent = state.caller_run_id();
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "waiting child", RunConfig::default())
        .unwrap();
    let first = runtime
        .request_user_question(child, "In the snapshot".into(), vec![], false)
        .unwrap();
    let request = request(vec![child], WaitMode::Any, 0);
    let result = observe(&runtime, parent, &request, receiver).await.unwrap();
    let later = runtime
        .request_user_question(child, "After the snapshot".into(), vec![], false)
        .unwrap();
    assert!(record_observations(
        &mut state,
        &runtime,
        &request,
        &result,
        &mut Observations::default()
    ));
    assert!(
        !state.reported_questions.contains(&first.id),
        "building a result is not presentation"
    );
    state.context.push_tool_result(
        "snapshot",
        tools::ToolResult::success(result.value.to_string()),
    );
    Observations {
        questions: vec![first.id.clone()],
        ..Default::default()
    }
    .presented(&mut state);
    assert!(state.reported_questions.contains(&first.id));
    assert!(!state.reported_questions.contains(&later.id));
    assert!(runtime.question_reported(&first.id));
    assert!(!runtime.question_reported(&later.id));
    let messages = runtime.take_inbox(parent).unwrap();
    assert_eq!(messages.len(), 1);
    assert!(messages[0].content.contains(&later.id));
    cleanup(&runtime).await;
}

// Event-driven semantic regressions. poll! checks a concrete predicate/future,
// never elapsed time or absence of an event over a timeout.
fn own_request(input: Value) -> WaitRequest {
    parse::<WaitArgs>(input).unwrap().validate().unwrap()
}

fn snapshot_reads() -> Vec<RunId> {
    SNAPSHOT_READS.with_borrow_mut(std::mem::take)
}

#[tokio::test]
async fn partial_all_and_unrelated_wakes_do_not_enrich_any_run_from_storage() {
    let directory = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: directory.path().join("query-count.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let (runtime, model) = fixture(None);
    let runtime = runtime.with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
    let (parent, first, second) = spawn(&runtime);
    model.first.notify_one();
    runtime.wait(first).await.unwrap();
    let request =
        own_request(json!({"run_ids":[first.to_string(), second.to_string()],"mode":"all"}));
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let waiting = observe(&runtime, parent, &request, receiver);
    tokio::pin!(waiting);
    snapshot_reads();
    assert!(poll!(&mut waiting).is_pending());
    for _ in 0..12 {
        runtime
            .shared
            .turn_version
            .send_modify(|version| *version += 1);
        runtime
            .shared
            .question_version
            .send_modify(|version| *version += 1);
        assert!(poll!(&mut waiting).is_pending());
    }
    assert_eq!(
        snapshot_reads(),
        [],
        "partial all is a memory-only predicate"
    );
    model.second.notify_one();
    runtime.wait(second).await.unwrap();
    let result = waiting.await.unwrap();
    assert_eq!(snapshot_reads(), [first, second]);
    assert_eq!(
        result["completed_run_ids"],
        json!([first.to_string(), second.to_string()])
    );
    cleanup(&runtime).await;
}

#[tokio::test]
async fn implicit_discovery_pages_all_unread_children_without_acknowledging_peeks() {
    let (runtime, _) = fixture(None);
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    let mut children = Vec::new();
    for _ in 0..11 {
        let child = runtime
            .delegate_background_as_child(parent, Role::Worker, "blocked", RunConfig::default())
            .unwrap();
        runtime.cancel(child).unwrap();
        runtime.wait(child).await.unwrap();
        children.push(child);
    }
    assert_eq!(runtime.wait_runs(parent, &[]).unwrap().len(), 11);
    let request = own_request(json!({}));
    snapshot_reads();
    let (ready, first) = events::poll(&runtime, parent, &request).unwrap();
    assert!(ready);
    assert_eq!(first["runs"].as_array().unwrap().len(), 8);
    assert_eq!(snapshot_reads().len(), 8);
    assert!(first.notices.iter().all(|notice| !notice.observed()));
    let (_, peek) = events::poll(&runtime, parent, &request).unwrap();
    assert_eq!(
        peek["completed_run_ids"], first["completed_run_ids"],
        "unpresented page is retained"
    );
    for notice in &first.notices {
        runtime.observe_completion(notice);
    }
    assert_eq!(runtime.wait_runs(parent, &[]).unwrap().len(), 3);
    let (_, second) = events::poll(&runtime, parent, &request).unwrap();
    assert_eq!(second["runs"].as_array().unwrap().len(), 3);
    let ids: HashSet<_> = first["completed_run_ids"]
        .as_array()
        .unwrap()
        .iter()
        .chain(second["completed_run_ids"].as_array().unwrap())
        .cloned()
        .collect();
    assert_eq!(ids.len(), children.len());
    for notice in &second.notices {
        runtime.observe_completion(notice);
    }
    assert!(runtime.wait_runs(parent, &[]).unwrap().is_empty());
    snapshot_reads();
    let (ready, empty) = events::poll(&runtime, parent, &request).unwrap();
    assert!(ready);
    assert_eq!(empty["status"], "no_wait_sources");
    assert_eq!(snapshot_reads(), []);
    cleanup(&runtime).await;
}

#[tokio::test]
async fn unread_message_precedes_no_sources_and_is_consumed_only_after_presentation() {
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) =
        crate::agent_loop::delegate_cleanup_tests::started_fixture(receiver).await;
    let parent = state.caller_run_id();
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "blocked", RunConfig::default())
        .unwrap();
    let id = runtime
        .send_agent_message(child, parent, AgentMessageKind::Send, "last message", None)
        .unwrap();
    runtime.cancel(child).unwrap();
    runtime.wait(child).await.unwrap();
    let notice = runtime
        .run_output(parent, child)
        .unwrap()
        .completion_notice
        .unwrap();
    runtime.observe_completion(&notice);
    state.transition(AgentRunPhase::Running, None).unwrap();
    let dispatch = wait(&mut state, &runtime, json!({})).await;
    assert!(!dispatch.result.is_error);
    assert_eq!(dispatch.result.progress, Some(tools::Progress::Advanced));
    let value: Value = serde_json::from_str(&dispatch.result.content).unwrap();
    assert_eq!(value["status"], "ready");
    assert_eq!(value["messages"][0]["message_id"], id);
    assert!(
        runtime
            .run_mailbox(parent)
            .unwrap()
            .has_interrupting_message()
    );
    // A dropped/cancelled result is not presented and cannot consume the ID.
    drop(dispatch);
    let dispatch = wait(&mut state, &runtime, json!({})).await;
    state.context.push_tool_result("receive", dispatch.result);
    dispatch.observations.presented(&mut state);
    assert!(
        !runtime
            .run_mailbox(parent)
            .unwrap()
            .has_interrupting_message()
    );
    for _ in 0..2 {
        let dispatch = wait(&mut state, &runtime, json!({})).await;
        assert!(!dispatch.result.is_error);
        assert_eq!(dispatch.result.progress, Some(tools::Progress::Stalled));
        let value: Value = serde_json::from_str(&dispatch.result.content).unwrap();
        assert_eq!(value["status"], "no_wait_sources");
        assert_eq!(value["reasons"].as_array().unwrap().len(), 5);
        assert_eq!(value["runs"], json!([]));
        assert_eq!(value["messages"], json!([]));
    }
    cleanup(&runtime).await;
}

#[tokio::test]
async fn parent_terminal_interrupts_every_form_without_output_or_repeat_edges() {
    for end in ["first", "failure", "stop"] {
        for form in 0..4 {
            let (runtime, model) = fixture(None);
            let parent =
                runtime.delegate_background(Role::Orchestrator, end.into(), RunConfig::default());
            let caller = runtime
                .delegate_background_as_child(parent, Role::Worker, "blocked", RunConfig::default())
                .unwrap();
            let child = runtime
                .delegate_background_as_child(caller, Role::Worker, "blocked", RunConfig::default())
                .unwrap();
            let input = match form {
                0 => json!({}),
                1 => json!({"run_id":parent.to_string()}),
                2 => json!({"run_ids":[child.to_string()],"mode":"any"}),
                _ => json!({"run_ids":[parent.to_string(), child.to_string()],"mode":"all"}),
            };
            let mut request = own_request(input);
            let (_cancel, receiver) = watch::channel(RunInterrupt::None);
            let result = {
                let waiting = observe(&runtime, caller, &request, receiver);
                tokio::pin!(waiting);
                assert!(poll!(&mut waiting).is_pending());
                if end == "stop" {
                    runtime.stop(parent, crate::StopScope::SelfOnly).unwrap();
                } else {
                    model.first.notify_one();
                }
                runtime.wait(parent).await.unwrap();
                snapshot_reads();
                waiting.await.unwrap()
            };
            assert_eq!(
                snapshot_reads(),
                [],
                "parent projection must never load its storage/output"
            );
            assert_eq!(result["events"][0]["kind"], "parent_terminated");
            assert_eq!(result["runs"].as_array().unwrap().len(), 1);
            assert_eq!(result["runs"][0]["run_id"], parent.to_string());
            assert!(result["runs"][0].get("output").is_none());
            assert_eq!(result["attention_question_ids"], json!([]));
            assert!(runtime.list_agents().iter().any(|run| run.run_id == caller
                && !matches!(
                    run.phase,
                    AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
                )));
            let key = result["events"][0]["identity"].as_str().unwrap().to_owned();
            request.observed.insert(key);
            let (ready, next) = events::poll(&runtime, caller, &request).unwrap();
            if form == 1 {
                assert!(ready);
                assert_eq!(next["runs"][0]["phase"], result["runs"][0]["phase"]);
                assert_eq!(next["events"], json!([]));
            } else {
                assert!(!ready, "an observed parent edge must not interrupt again");
            }
            cleanup(&runtime).await;
        }
    }
}

#[tokio::test]
async fn parent_idle_and_questions_are_neither_readiness_nor_observations() {
    let directory = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: directory.path().join("parent.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let (runtime, model) = fixture(None);
    let runtime = runtime.with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "first".into(),
        RunConfig {
            interactive: true,
            keep_alive: true,
            ..Default::default()
        },
    );
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "blocked", RunConfig::default())
        .unwrap();
    let question = runtime
        .request_user_question(parent, "private parent question".into(), vec![], false)
        .unwrap();
    idle_turn(&runtime, &model, parent, true).await;
    for input in [
        json!({}),
        json!({"run_id":parent.to_string()}),
        json!({"run_ids":[parent.to_string()], "mode":"all"}),
    ] {
        let request = own_request(input);
        snapshot_reads();
        let (ready, result) = events::poll(&runtime, child, &request).unwrap();
        assert!(!ready);
        assert_eq!(snapshot_reads(), []);
        assert_eq!(result["runs"], json!([]));
        assert!(!runtime.question_reported(&question.id));
        assert!(
            runtime
                .user_question(&question.id)
                .unwrap()
                .unwrap()
                .answer
                .is_none()
        );
    }
    cleanup(&runtime).await;
}

#[tokio::test]
async fn legacy_phase_only_keeps_unpresented_completion_unread() {
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) =
        crate::agent_loop::delegate_cleanup_tests::started_fixture(receiver).await;
    let parent = state.caller_run_id();
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "blocked", RunConfig::default())
        .unwrap();
    runtime.cancel(child).unwrap();
    runtime.wait(child).await.unwrap();
    let notice = runtime
        .run_output(parent, child)
        .unwrap()
        .completion_notice
        .unwrap();
    state.transition(AgentRunPhase::Running, None).unwrap();
    let dispatch = wait(&mut state, &runtime, json!({"run_id":child.to_string()})).await;
    assert_eq!(dispatch.result.content, "Error");
    assert!(dispatch.observations.completions.is_empty());
    state.context.push_tool_result("legacy", dispatch.result);
    dispatch.observations.presented(&mut state);
    assert!(!notice.observed());
    let repeated = wait(&mut state, &runtime, json!({"run_id":child.to_string()})).await;
    assert_eq!(repeated.result.content, "Error");
    assert_eq!(repeated.result.progress, Some(tools::Progress::Stalled));
    assert!(repeated.observations.completions.is_empty());
    state
        .context
        .push_tool_result("legacy-repeat", repeated.result);
    repeated.observations.presented(&mut state);
    assert!(!notice.observed());
    let dispatch = wait(&mut state, &runtime, json!({})).await;
    assert_eq!(
        serde_json::from_str::<Value>(&dispatch.result.content).unwrap()["completed_run_ids"],
        json!([child.to_string()])
    );
    assert!(!notice.observed());
    state
        .context
        .push_tool_result("structured", dispatch.result);
    dispatch.observations.presented(&mut state);
    assert!(notice.observed());
    cleanup(&runtime).await;
}

fn queue_lane(
    runtime: &AgentRuntime,
    root: RunId,
    id: &str,
) -> tokio::sync::oneshot::Receiver<Result<usize, storage::lane_delivery::LaneDeliveryNack>> {
    let (result, receiver) = tokio::sync::oneshot::channel();
    let claim = storage::lane_delivery::LaneDeliveryClaim {
        destination_thread_id: "destination".into(),
        destination_epoch: 1,
        claim_epoch: 1,
        token: format!("token-{id}"),
        lease_until_ns: i64::MAX,
        owner: storage::lane_delivery::LaneDeliveryOwner {
            root_run_id: root.to_string(),
            generation: 1,
            ownership_generation: 0,
        },
        completed_root_wake: false,
        notifications: vec![event_bus::lane::LaneNotification {
            notification_id: id.into(),
            lane_id: "lane".into(),
            management_revision: 1,
            source_thread_id: "source".into(),
            destination_thread_id: "destination".into(),
            kind: event_bus::lane::LaneNotificationKind::Milestone,
            report_seq: Some(1),
            created_at_ns: 0,
        }],
    };
    runtime
        .shared
        .lanes
        .delivery
        .pending
        .lock()
        .unwrap()
        .insert(
            root,
            crate::lane_delivery::PendingDelivery {
                claim: crate::lane_delivery::outbox::DeliveryClaim::Lane(claim),
                result,
            },
        );
    runtime
        .shared
        .lanes
        .delivery
        .version
        .send_modify(|version| *version += 1);
    receiver
}

#[tokio::test]
async fn lane_readiness_interrupts_all_forms_without_consuming_claim_or_other_root() {
    for preexisting in [false, true] {
        for form in 0..3 {
            let (runtime, _) = fixture(None);
            let (parent, caller, other) = spawn(&runtime);
            let child = runtime
                .delegate_background_as_child(caller, Role::Worker, "blocked", RunConfig::default())
                .unwrap();
            let input = match form {
                0 => json!({}),
                1 => json!({"run_id":parent.to_string()}),
                _ => json!({"run_ids":[child.to_string()],"mode":"all"}),
            };
            let request = own_request(input);
            let (_cancel, receiver) = watch::channel(RunInterrupt::None);
            let waiting = observe(&runtime, caller, &request, receiver);
            tokio::pin!(waiting);
            let mut other_ack = queue_lane(&runtime, other, "other-root-notice");
            assert!(
                poll!(&mut waiting).is_pending(),
                "another root's version/claim is not readiness"
            );
            if !preexisting {
                assert!(poll!(&mut waiting).is_pending());
            }
            let mut ack = queue_lane(&runtime, caller, "own-notice");
            // preexisting is covered by a fresh wait subscription below; otherwise
            // the active future has already subscribed before the claim insertion.
            let result = if preexisting {
                let (_cancel, rx) = watch::channel(RunInterrupt::None);
                observe(&runtime, caller, &request, rx).await.unwrap()
            } else {
                waiting.await.unwrap()
            };
            assert_eq!(
                result["events"],
                json!([{"kind":"lane", "notification_id":"own-notice", "identity":"lane:own-notice"}])
            );
            assert_eq!(result["runs"], json!([]));
            assert!(matches!(
                ack.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ));
            assert!(matches!(
                other_ack.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ));
            assert_eq!(runtime.wait_lane_deliveries(caller), ["own-notice"]);
            assert_eq!(runtime.wait_lane_deliveries(other), ["other-root-notice"]);
            cleanup(&runtime).await;
        }
    }
}

#[tokio::test]
async fn managed_lane_source_disappearance_returns_no_wait_sources() {
    let (runtime, _) = fixture(None);
    let caller =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    runtime
        .shared
        .lanes
        .delivery
        .wait_lanes
        .lock()
        .unwrap()
        .insert(caller, vec!["managed".into()]);
    let request = own_request(json!({}));
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let waiting = observe(&runtime, caller, &request, receiver);
    tokio::pin!(waiting);
    assert!(poll!(&mut waiting).is_pending());
    runtime
        .shared
        .lanes
        .delivery
        .wait_lanes
        .lock()
        .unwrap()
        .remove(&caller);
    runtime
        .shared
        .lanes
        .delivery
        .version
        .send_modify(|version| *version += 1);
    let result = waiting.await.unwrap();
    assert_eq!(result["status"], "no_wait_sources");
    assert_eq!(result["events"], json!([]));
    cleanup(&runtime).await;
}

#[tokio::test]
async fn subagent_question_listing_commits_receipts_only_when_presented() {
    let directory = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: directory.path().join("listing.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) =
        crate::agent_loop::delegate_cleanup_tests::started_fixture(receiver).await;
    let runtime = runtime.with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
    let parent = state.caller_run_id();
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "blocked", RunConfig::default())
        .unwrap();
    let question = runtime
        .request_user_question(child, "preserve cancelled listing".into(), vec![], false)
        .unwrap();
    let args = json!({"run_id":child.to_string()});
    let dropped = crate::meta::questions::subagent_questions(&mut state, &runtime, args.clone());
    assert!(!dropped.result.is_error);
    drop(dropped);
    assert!(!runtime.question_reported(&question.id));
    assert!(!state.reported_questions.contains(&question.id));
    let dispatch = crate::meta::questions::subagent_questions(&mut state, &runtime, args);
    let listed: Value = serde_json::from_str(&dispatch.result.content).unwrap();
    assert_eq!(listed[0]["id"], question.id);
    state.context.push_tool_result("listed", dispatch.result);
    dispatch.observations.presented(&mut state);
    assert!(runtime.question_reported(&question.id));
    assert!(state.reported_questions.contains(&question.id));
    assert!(runtime.take_inbox(parent).unwrap().is_empty());
    cleanup(&runtime).await;
}

#[tokio::test]
async fn host_input_is_ready_even_after_the_last_source_disappears() {
    let (runtime, _) = fixture(None);
    let caller =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    let request = own_request(json!({}));
    let result = user_input_response(&runtime, caller, &request).unwrap();
    assert_eq!(result["status"], "ready");
    assert_eq!(result["user_input_ready"], true);
    assert_eq!(result["events"], json!([{"kind":"user_input"}]));
    assert!(result.get("reasons").is_none());
    cleanup(&runtime).await;
}

#[tokio::test]
async fn dropped_completion_result_does_not_spend_its_progress_identity() {
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) =
        crate::agent_loop::delegate_cleanup_tests::started_fixture(receiver).await;
    let parent = state.caller_run_id();
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "blocked", RunConfig::default())
        .unwrap();
    runtime.cancel(child).unwrap();
    runtime.wait(child).await.unwrap();
    let notice = runtime
        .run_output(parent, child)
        .unwrap()
        .completion_notice
        .unwrap();
    state.transition(AgentRunPhase::Running, None).unwrap();
    let dropped = wait(&mut state, &runtime, json!({})).await;
    assert_eq!(dropped.result.progress, Some(tools::Progress::Advanced));
    drop(dropped);
    assert!(!notice.observed());
    let shown = wait(&mut state, &runtime, json!({})).await;
    assert_eq!(
        shown.result.progress,
        Some(tools::Progress::Advanced),
        "an unpresented completion cannot spend progress"
    );
    state.context.push_tool_result("shown", shown.result);
    shown.observations.presented(&mut state);
    assert!(notice.observed());
    let repeated = wait(&mut state, &runtime, json!({"run_ids":[child.to_string()]})).await;
    assert_eq!(repeated.result.progress, Some(tools::Progress::Stalled));
    cleanup(&runtime).await;
}

#[tokio::test]
async fn distinct_lane_ids_advance_but_repeated_readiness_does_not_ack_or_advance() {
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) =
        crate::agent_loop::delegate_cleanup_tests::started_fixture(receiver).await;
    let root = state.caller_run_id();
    state.transition(AgentRunPhase::Running, None).unwrap();
    for n in 0..12 {
        let id = format!("notice-{n}");
        let mut ack = queue_lane(&runtime, root, &id);
        let shown = wait(&mut state, &runtime, json!({})).await;
        assert_eq!(shown.result.progress, Some(tools::Progress::Advanced));
        assert!(!state.budget.wait_event_observed(&format!("lane:{id}")));
        state
            .context
            .push_tool_result(format!("lane-{n}"), shown.result);
        shown.observations.presented(&mut state);
        assert!(state.budget.wait_event_observed(&format!("lane:{id}")));
        let repeated = wait(&mut state, &runtime, json!({})).await;
        assert_eq!(repeated.result.progress, Some(tools::Progress::Stalled));
        assert!(matches!(
            ack.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(runtime.wait_lane_deliveries(root), [id]);
        runtime
            .shared
            .lanes
            .delivery
            .pending
            .lock()
            .unwrap()
            .remove(&root);
    }
    cleanup(&runtime).await;
}

#[tokio::test]
async fn host_interrupt_of_partial_all_enriches_only_the_event_child() {
    let (runtime, model) = fixture(None);
    let (parent, first, second) = spawn(&runtime);
    model.first.notify_one();
    runtime.wait(first).await.unwrap();
    let request =
        own_request(json!({"run_ids":[first.to_string(),second.to_string()],"mode":"all"}));
    snapshot_reads();
    assert!(!events::poll(&runtime, parent, &request).unwrap().0);
    assert_eq!(snapshot_reads(), []);
    let result = user_input_response(&runtime, parent, &request).unwrap();
    assert_eq!(snapshot_reads(), [first]);
    assert_eq!(result["user_input_ready"], true);
    assert_eq!(result["completed_run_ids"], json!([first.to_string()]));
    assert_eq!(result["runs"].as_array().unwrap().len(), 1);
    assert!(result.notices.iter().all(|notice| !notice.observed()));
    cleanup(&runtime).await;
}

#[tokio::test]
async fn delegate_wave_progress_is_committed_per_presented_child() {
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) =
        crate::agent_loop::delegate_cleanup_tests::started_fixture(receiver).await;
    let parent = state.caller_run_id();
    let mut children = Vec::new();
    for _ in 0..2 {
        let child = runtime
            .delegate_background_as_child(parent, Role::Worker, "blocked", RunConfig::default())
            .unwrap();
        runtime.cancel(child).unwrap();
        runtime.wait(child).await.unwrap();
        children.push(child);
    }
    state.transition(AgentRunPhase::Running, None).unwrap();
    let mut wave = crate::meta::delegation::wait_delegates(
        &mut state,
        &runtime,
        children.iter().copied().map(Ok).collect(),
    )
    .await;
    assert_eq!(wave.len(), 2);
    let identities: Vec<_> = wave
        .iter()
        .map(|dispatch| dispatch.observations.child_progress[0].identity())
        .collect();
    let first = wave.remove(0);
    assert!(!first.result.is_error);
    assert_eq!(first.observations.child_progress.len(), 1);
    assert_eq!(
        first.observations.child_progress[0].run_id,
        children[0].to_string()
    );
    assert!(
        first.observations.completions.is_empty(),
        "phase-only must not ACK output"
    );
    for identity in &identities {
        assert!(!state.budget.wait_event_observed(identity));
    }
    state.context.push_tool_result("first", first.result);
    first.observations.presented(&mut state);
    assert!(state.budget.wait_event_observed(&identities[0]));
    assert!(!state.budget.wait_event_observed(&identities[1]));
    drop(wave);
    let second = wait(
        &mut state,
        &runtime,
        json!({"run_ids":[children[1].to_string()]}),
    )
    .await;
    assert_eq!(second.result.progress, Some(tools::Progress::Advanced));
    cleanup(&runtime).await;
}

#[tokio::test]
async fn completion_presented_by_run_output_does_not_mint_wait_progress() {
    let (_cancel, receiver) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) =
        crate::agent_loop::delegate_cleanup_tests::started_fixture(receiver).await;
    let parent = state.caller_run_id();
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "blocked", RunConfig::default())
        .unwrap();
    runtime.cancel(child).unwrap();
    runtime.wait(child).await.unwrap();
    state.transition(AgentRunPhase::Running, None).unwrap();
    let output =
        crate::meta::runs::run_output(&mut state, &runtime, json!({"run_id":child.to_string()}));
    assert!(!output.result.is_error);
    state.context.push_tool_result("output", output.result);
    output.observations.presented(&mut state);
    let repeated = wait(&mut state, &runtime, json!({"run_ids":[child.to_string()]})).await;
    assert_eq!(repeated.result.progress, Some(tools::Progress::Stalled));
    cleanup(&runtime).await;
}
