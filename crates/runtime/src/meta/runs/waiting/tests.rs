use std::sync::Arc;

use agents::Role;
use event_bus::{AgentMessageKind, EventBus};
use futures_util::poll;
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec, Usage};
use tokio::sync::Notify;
use tools::ToolExecutor;

use super::*;
use crate::{AgentInvocationContext, AgentModel, RunConfig, RuntimeError};

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

fn request(runs: Vec<RunId>, mode: WaitMode, timeout_ms: u64) -> WaitRequest {
    WaitRequest {
        runs,
        mode,
        timeout: Duration::from_millis(timeout_ms),
        legacy: false,
    }
}

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
        json!({"run_id":"run-1"}),
        json!({"run_ids":["run-1","run-2"],"mode":"all","timeout_ms":0}),
        json!({"run_ids":["run-1"],"timeout_ms":60001}),
        json!({"run_ids":["run-1"],"timeout_ms":600000}),
    ] {
        assert!(validator.is_valid(&input));
        assert!(parse::<WaitArgs>(input).unwrap().validate().is_ok());
    }
    for input in [
        json!({}),
        json!({"run_id":"run-1","run_ids":["run-2"]}),
        json!({"run_ids":[]}),
        json!({"run_ids":["run-1","run-1"]}),
        json!({"run_id":"run-1","timeout_ms":600001}),
        json!({"run_id":"run-1","timeout_ms":-1}),
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
    let (_cancel, receiver) = watch::channel(false);
    let request = request(vec![first, second], WaitMode::Any, 60000);
    let wait = observe(&runtime, parent, &request, receiver);
    tokio::pin!(wait);
    assert!(poll!(&mut wait).is_pending());
    model.first.notify_one();
    runtime.wait(first).await.unwrap();
    let result = wait.await.unwrap();
    assert_eq!(result["timed_out"], false);
    assert_eq!(result["completed_run_ids"], json!([first.to_string()]));
    assert_eq!(result["runs"][1]["status"], "still_running");
    assert_eq!(result["runs"][0]["output_truncated"], true);
    assert!(result["runs"][0]["output"].as_str().unwrap().len() <= OUTPUT_BYTES);
    // Observing completion must not consume the parent's completion relay.
    assert!(!runtime.take_inbox(parent).unwrap().is_empty());
    cleanup(&runtime).await;
}

#[tokio::test(start_paused = true)]
async fn all_waits_for_every_completion_including_cancelled_children() {
    let (runtime, model) = fixture(None);
    let (parent, first, second) = spawn(&runtime);
    let (_cancel, receiver) = watch::channel(false);
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
    let (_cancel, receiver) = watch::channel(false);
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
    let (_cancel, receiver) = watch::channel(false);
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
    let (cancel, receiver) = watch::channel(false);
    let request = request(vec![first], WaitMode::Any, 60000);
    let wait = observe(&runtime, parent, &request, receiver.clone());
    tokio::pin!(wait);
    assert!(poll!(&mut wait).is_pending());
    model.first.notify_one();
    runtime.wait(first).await.unwrap();
    cancel.send_replace(true);
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
        let (_cancel, receiver) = watch::channel(false);
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
    let (_cancel, receiver) = watch::channel(false);
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
    let (_cancel, receiver) = watch::channel(false);
    assert_eq!(
        observe(
            &runtime,
            parent,
            &request(vec![child], WaitMode::Any, 0),
            receiver.clone()
        )
        .await
        .unwrap()["runs"][0]["phase"],
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
    let (_cancel, receiver) = watch::channel(false);
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
    let (_cancel, receiver) = watch::channel(false);
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
    let (_cancel, receiver) = watch::channel(false);
    let request = request(vec![first, second], WaitMode::All, 60000);
    let wait = observe(&runtime, parent, &request, receiver.clone());
    tokio::pin!(wait);
    assert!(poll!(&mut wait).is_pending());
    let question = runtime
        .request_user_question(second, "Optional preference".into(), vec![], false)
        .unwrap();
    let result = wait.await.unwrap();
    assert_eq!(result["attention_run_ids"], json!([second.to_string()]));
    assert_eq!(result["runs"][1]["needs_user_input"], false);
    assert_eq!(result["runs"][1]["has_pending_question"], true);
    assert_eq!(
        runtime.subagent_questions(parent, second).unwrap(),
        vec![question.clone()]
    );
    runtime
        .answer_subagent_question(parent, &question.id, "Use A")
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
    let (_cancel, receiver) = watch::channel(false);
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
async fn default_wait_suspends_for_ten_minutes_without_polling_or_cancelling() {
    let (runtime, _) = fixture(None);
    let (parent, first, _) = spawn(&runtime);
    let (_cancel, receiver) = watch::channel(false);
    let request = parse::<WaitArgs>(json!({"run_ids":[first.to_string()]}))
        .unwrap()
        .validate()
        .unwrap();
    let began = tokio::time::Instant::now();
    let wait = observe(&runtime, parent, &request, receiver);
    tokio::pin!(wait);
    assert!(poll!(&mut wait).is_pending());
    tokio::time::advance(Duration::from_secs(60)).await;
    assert!(poll!(&mut wait).is_pending());
    let result = wait.await.unwrap();
    assert_eq!(began.elapsed(), Duration::from_secs(600));
    assert_eq!(result["timed_out"], true);
    assert_eq!(result["inbox_ready"], false);
    assert_eq!(result["runs"][0]["status"], "still_running");
    cleanup(&runtime).await;
}

#[tokio::test(start_paused = true)]
async fn message_interrupts_all_wait_without_consuming_or_copying_the_inbox() {
    let (runtime, _) = fixture(None);
    let (parent, first, second) = spawn(&runtime);
    let (_cancel, receiver) = watch::channel(false);
    let request = request(vec![first, second], WaitMode::All, MAX_WAIT_MS);
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
    let (_cancel, receiver) = watch::channel(false);
    let request = request(vec![first], WaitMode::Any, MAX_WAIT_MS);
    let wait = observe(&runtime, parent, &request, receiver);
    tokio::pin!(wait);
    assert!(poll!(&mut wait).is_pending());
    runtime
        .send_agent_message(second, parent, AgentMessageKind::Send, "Finding", None)
        .unwrap();
    let result = wait.await.unwrap();
    assert_eq!(result["inbox_ready"], true);
    assert_eq!(result["timed_out"], false);
    assert_eq!(result["runs"][0]["status"], "still_running");
    cleanup(&runtime).await;
}

#[tokio::test(start_paused = true)]
async fn cancellation_wins_over_a_simultaneous_inbox_message() {
    let (runtime, _) = fixture(None);
    let (parent, first, _) = spawn(&runtime);
    let (cancel, receiver) = watch::channel(false);
    let request = request(vec![first], WaitMode::Any, MAX_WAIT_MS);
    let wait = observe(&runtime, parent, &request, receiver);
    tokio::pin!(wait);
    assert!(poll!(&mut wait).is_pending());
    runtime
        .send_agent_message(first, parent, AgentMessageKind::Send, "Finding", None)
        .unwrap();
    cancel.send_replace(true);
    assert_eq!(wait.await.unwrap_err(), "wait cancelled");
    assert_eq!(runtime.take_inbox(parent).unwrap().len(), 1);
    cleanup(&runtime).await;
}
