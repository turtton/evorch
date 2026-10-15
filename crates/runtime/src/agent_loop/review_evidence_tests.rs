use crate::escalation_review::support;

use crate::{AgentModel, AgentRuntime, Role, RunConfig, RunStore};
use event_bus::{EventBus, EventKind, MessageEvent};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use support::{ScriptedModel, text_response, tool_response};

struct Fixture {
    _dir: tempfile::TempDir,
    _storage: storage::Storage,
    runtime: AgentRuntime,
    events: event_bus::EventReceiver,
}
impl Fixture {
    fn new(model: Arc<dyn AgentModel>, executor: Option<tools::ToolExecutor>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("review.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(256));
        let events = bus.subscribe();
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(executor.unwrap_or_else(|| tools::ToolExecutor::new(bus))),
            model,
        )
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        Self {
            _dir: dir,
            _storage: storage,
            runtime,
            events,
        }
    }
    fn chat(&self) -> crate::RunId {
        self.runtime
            .delegate_chat(
                "chat",
                Role::Orchestrator,
                "Investigate the public docs".into(),
                RunConfig {
                    interactive: true,
                    ..Default::default()
                },
            )
            .unwrap()
    }
    async fn completed(&mut self) {
        loop {
            if matches!(
                self.events.recv().await.unwrap().kind,
                EventKind::Message(MessageEvent::MessageCompleted { .. })
            ) {
                break;
            }
        }
    }
}

struct RecordingReviewer {
    model: ScriptedModel,
    requests: tokio::sync::mpsc::UnboundedSender<Vec<Message>>,
    release: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl AgentModel for RecordingReviewer {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "recording".into()
    }
    async fn complete(
        &self,
        invocation: &crate::AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, crate::RuntimeError> {
        if invocation.purpose == event_bus::RequestPurpose::EscalationReview {
            self.requests.send(messages.to_vec()).unwrap();
            self.release.notified().await;
            Ok(text_response(
                r#"{"approve":true,"risk_level":"low","authorization_level":"high"}"#,
                FinishReason::Stop,
            ))
        } else {
            self.model.complete(invocation, role, messages, tools).await
        }
    }
}

#[tokio::test]
async fn clean_stop_reference_reaches_evidence_materialization_and_recording_reviewer() {
    let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
    let mut explanation = text_response("May I ", FinishReason::Stop);
    explanation.message.content.push(ContentBlock::Text {
        text: "read the docs?".into(),
    });
    let model = Arc::new(RecordingReviewer {
        model: ScriptedModel::new([Ok(explanation)]),
        requests,
        release: tokio::sync::Notify::new(),
    });
    let mut fixture = Fixture::new(model.clone(), None);
    let run = fixture.chat();
    fixture.completed().await;
    assert_eq!(
        fixture
            .runtime
            .peek_pending_reference(run)
            .unwrap()
            .unwrap()
            .context,
        "May I read the docs?"
    );
    fixture.runtime.send_message(run, "いいよ".into()).unwrap();
    assert!(
        fixture
            .runtime
            .peek_pending_reference(run)
            .unwrap()
            .is_none()
    );
    let store = fixture.runtime.shared.run_store.get().unwrap();
    let row = store
        .review_evidence_for_lineage(&[run.to_string()])
        .unwrap()
        .into_iter()
        .find(|r| r.kind == "host_chat")
        .unwrap();
    assert_eq!(row.reference_kind.as_deref(), Some("assistant_turn"));
    let reference: crate::escalation_review::ReferenceContext =
        serde_json::from_str(row.reference_context.as_ref().unwrap()).unwrap();
    assert_eq!(reference.text, "May I read the docs?");
    assert_eq!(reference.source_run_id, run.to_string());
    let materialized = crate::run_store::materialize_review_context(store, run).unwrap();
    let live = fixture.runtime.shared.review_runs.lock().unwrap()[&run].clone();
    for review in [live, materialized] {
        let (context, _) =
            crate::escalation_review::review_context(review, None, None, "").unwrap();
        let reviewer = crate::escalation_review::QuickModelReviewer::new(model.clone());
        let id = run.to_string();
        let task = tokio::spawn(async move {
            reviewer
                .review_web_fetch(
                    &id,
                    &serde_json::json!({"url":"https://example.com"}),
                    Some(context),
                )
                .await
        });
        let messages = received.recv().await.unwrap();
        let ContentBlock::Text { text } = &messages[1].content[0] else {
            panic!("payload")
        };
        let payload: serde_json::Value = serde_json::from_str(text).unwrap();
        let request = payload["context"]["real_user_requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["text"] == "いいよ")
            .unwrap();
        assert_eq!(request["reference_context"]["text"], "May I read the docs?");
        model.release.notify_one();
        assert!(task.await.unwrap().is_ok());
    }
    fixture.runtime.cancel(run).unwrap();
}

#[tokio::test]
async fn only_clean_stop_captures_and_new_capture_overwrites_without_stale_take() {
    for (finish, with_tool) in [
        (FinishReason::ToolUse, false),
        (FinishReason::Length, false),
        (FinishReason::Stop, true),
    ] {
        for prior in [None, Some("previous clean stop")] {
            let mut response = text_response("not a reference", finish.clone());
            if with_tool {
                response.message.content.push(ContentBlock::ToolUse {
                    id: "read".into(),
                    name: "user_answers".into(),
                    input: serde_json::json!({}),
                });
            }
            let gate = Arc::new(tokio::sync::Notify::new());
            let model = Arc::new(ScriptedModel::gated([Ok(response)], gate.clone()));
            let mut fixture = Fixture::new(model.clone(), None);
            let run = fixture.chat();
            if let Some(prior) = prior {
                fixture
                    .runtime
                    .capture_pending_reference(run, prior.into())
                    .unwrap();
            }
            model.wait_for_request(0).await;
            gate.notify_one();
            fixture.completed().await;
            assert_eq!(
                fixture
                    .runtime
                    .peek_pending_reference(run)
                    .unwrap()
                    .map(|pending| pending.context),
                prior.map(str::to_string)
            );
            fixture.runtime.cancel(run).unwrap();
        }
    }
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "new reference",
        FinishReason::Stop,
    ))]));
    let mut fixture = Fixture::new(model, None);
    let run = fixture.chat();
    let old = fixture
        .runtime
        .capture_pending_reference(run, "old reference".into())
        .unwrap();
    fixture.completed().await;
    assert!(
        fixture
            .runtime
            .take_pending_reference(run, old)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fixture
            .runtime
            .peek_pending_reference(run)
            .unwrap()
            .unwrap()
            .context,
        "new reference"
    );
    fixture
        .runtime
        .send_internal_message(run, "internal".into())
        .unwrap();
    assert!(
        fixture
            .runtime
            .peek_pending_reference(run)
            .unwrap()
            .is_some()
    );
    fixture.runtime.cancel(run).unwrap();
}

struct ProbeFetch(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl tools::Tool for ProbeFetch {
    fn name(&self) -> &'static str {
        "web_fetch"
    }
    fn description(&self) -> &'static str {
        "probe"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    fn permissions(&self) -> tools::Permissions {
        tools::Permissions::network()
    }
    async fn execute(&self, _: serde_json::Value) -> Result<tools::ToolResult, tools::ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(tools::ToolResult::success("fetched"))
    }
}

#[tokio::test]
async fn web_approval_rechecks_revision_after_host_input() {
    for change in ["host", "unsynced", "unchanged"] {
        let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
        let model = Arc::new(RecordingReviewer {
            model: ScriptedModel::new([
                Ok(tool_response(
                    "fetch",
                    "web_fetch",
                    serde_json::json!({"url":"https://example.com"}),
                )),
                Ok(text_response("done", FinishReason::Stop)),
            ]),
            requests,
            release: tokio::sync::Notify::new(),
        });
        let executed = Arc::new(AtomicUsize::new(0));
        let mut executor = tools::ToolExecutor::new(Arc::new(EventBus::new(64)));
        executor
            .register(Arc::new(ProbeFetch(executed.clone())))
            .unwrap();
        let fixture = Fixture::new(model.clone(), Some(executor));
        let run = fixture.chat();
        received.recv().await.unwrap();
        match change {
            "host" => fixture
                .runtime
                .send_message(run, "Do not fetch any URLs".into())
                .unwrap(),
            "unsynced" => fixture.runtime.shared.review_runs.lock().unwrap()[&run].mark_unsynced(),
            _ => {}
        }
        model.release.notify_one();
        let messages = model.model.wait_for_request(1).await;
        assert_eq!(
            executed.load(Ordering::SeqCst),
            usize::from(change == "unchanged")
        );
        if change != "unchanged" {
            assert!(messages.iter().flat_map(|m| &m.content).any(|block| matches!(block, ContentBlock::ToolResult { content, is_error: true, .. } if content.iter().any(|c| matches!(c, providers::ToolResultContent::Text { text } if text.contains("review context changed during review"))))));
        }
        fixture.runtime.cancel(run).unwrap();
    }
}

#[tokio::test]
async fn oversized_capture_is_not_truncated_and_failed_send_keeps_pending() {
    let text = "a".repeat(256 * 1024 + 1);
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        &text,
        FinishReason::Stop,
    ))]));
    let mut fixture = Fixture::new(model, None);
    let run = fixture.chat();
    fixture.completed().await;
    assert_eq!(
        fixture
            .runtime
            .peek_pending_reference(run)
            .unwrap()
            .unwrap()
            .context,
        text
    );
    let before = fixture.runtime.shared.review_runs.lock().unwrap()[&run].stamp();
    assert!(fixture.runtime.send_message(run, "yes".into()).is_err());
    assert_eq!(
        fixture.runtime.shared.review_runs.lock().unwrap()[&run].stamp(),
        before
    );
    assert!(
        fixture
            .runtime
            .peek_pending_reference(run)
            .unwrap()
            .is_some()
    );
    assert_eq!(fixture.runtime.follow_up_status(run).unwrap().pending, 0);
    assert!(
        !fixture
            .runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .review_evidence_for_lineage(&[run.to_string()])
            .unwrap()
            .iter()
            .any(|row| row.kind == "host_chat")
    );
    fixture.runtime.cancel(run).unwrap();
}
