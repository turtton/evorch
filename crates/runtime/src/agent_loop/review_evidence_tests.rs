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
        Self::with_storage(model, executor, true)
    }
    fn with_storage(
        model: Arc<dyn AgentModel>,
        executor: Option<tools::ToolExecutor>,
        attach: bool,
    ) -> Self {
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
        );
        let runtime = if attach {
            runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap())
        } else {
            runtime
        };
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
        let (context, _) = crate::escalation_review::review_context(review, None).unwrap();
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
async fn rejected_host_evidence_keeps_the_original_reference_pending() {
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "May I read the docs?",
        FinishReason::Stop,
    ))]));
    let mut fixture = Fixture::new(model, None);
    let run = fixture.chat();
    fixture.completed().await;
    let before = fixture
        .runtime
        .peek_pending_reference(run)
        .unwrap()
        .unwrap();
    assert!(
        fixture
            .runtime
            .send_message(run, "a".repeat(256 * 1024 + 1))
            .is_err()
    );
    assert_eq!(
        fixture.runtime.peek_pending_reference(run).unwrap(),
        Some(before.clone())
    );
    let store = fixture.runtime.shared.run_store.get().unwrap();
    assert!(
        !store
            .handle
            .assistant_reference(&before.reference_id)
            .unwrap()
            .unwrap()
            .consumed
    );
    assert_eq!(fixture.runtime.follow_up_status(run).unwrap().pending, 0);
    fixture.runtime.send_message(run, "yes".into()).unwrap();
    assert!(
        store
            .handle
            .assistant_reference(&before.reference_id)
            .unwrap()
            .unwrap()
            .consumed
    );
    fixture.runtime.cancel(run).unwrap();
}

#[tokio::test]
async fn rejected_new_capture_cannot_reuse_a_superseded_explanation() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(ScriptedModel::gated(
        [Ok(text_response("later", FinishReason::Stop))],
        gate.clone(),
    ));
    let fixture = Fixture::new(model.clone(), None);
    let run = fixture.chat();
    model.wait_for_request(0).await;
    fixture
        .runtime
        .capture_pending_reference(run, "old explanation".into())
        .unwrap();
    let old = fixture
        .runtime
        .peek_pending_reference(run)
        .unwrap()
        .unwrap();
    assert!(
        fixture
            .runtime
            .capture_pending_reference(run, "b".repeat(256 * 1024 + 1))
            .is_err()
    );
    assert!(
        fixture
            .runtime
            .peek_pending_reference(run)
            .unwrap()
            .is_none()
    );
    let store = fixture.runtime.shared.run_store.get().unwrap();
    assert!(
        store
            .handle
            .assistant_reference(&old.reference_id)
            .unwrap()
            .unwrap()
            .consumed
    );
    fixture.runtime.send_message(run, "yes".into()).unwrap();
    let rows = store
        .review_evidence_for_lineage(&[run.to_string()])
        .unwrap();
    assert!(
        rows.iter()
            .find(|row| row.text == "yes")
            .unwrap()
            .reference_context
            .is_none()
    );
    fixture.runtime.cancel(run).unwrap();
}

#[tokio::test]
async fn approval_explanation_keeps_source_provenance_after_compaction_handoff_and_restart() {
    let model = Arc::new(ScriptedModel::new([
        Ok(text_response(
            "ORIGINAL explanation: may I perform the proposed check?",
            FinishReason::Stop,
        )),
        Ok(tool_response(
            "escalate",
            "escalate",
            serde_json::json!({"original_request":"investigate", "escalation_reason":"coordinate"}),
        )),
        Err(crate::RuntimeError::Model {
            reason: "receiver unavailable".into(),
        }),
    ]));
    let mut fixture = Fixture::new(model, None);
    fixture.runtime = fixture.runtime.with_compaction(config::CompactionConfig {
        context_window_tokens: 1_000_000,
        keep_recent_tokens: 1,
        max_summary_bytes: 16,
        summarizer: config::SummarizerKind::Structural,
        ..Default::default()
    });
    let source = fixture
        .runtime
        .delegate_chat(
            "source-reference",
            Role::Worker,
            "real original request".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    loop {
        if matches!(
            fixture.events.recv().await.unwrap().kind,
            EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                to: event_bus::AgentRunPhase::Waiting,
                ..
            })
        ) {
            break;
        }
    }
    let original = fixture
        .runtime
        .peek_pending_reference(source)
        .unwrap()
        .unwrap();
    fixture
        ._storage
        .handle()
        .append_run_ledger(
            &source.to_string(),
            "summary is not the approval explanation",
        )
        .unwrap();
    fixture.runtime.compact(source).unwrap();
    fixture
        .runtime
        .send_internal_message(source, "coordinate the remaining work".into())
        .unwrap();
    let target = loop {
        match fixture.events.recv().await.unwrap().kind {
            EventKind::Lifecycle(event_bus::LifecycleEvent::EscalationRequested {
                new_run_id,
                ..
            }) => break new_run_id.parse::<crate::RunId>().unwrap(),
            EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                run_id,
                to: event_bus::AgentRunPhase::Error,
                reason,
                ..
            }) if run_id == source.to_string() => panic!("source failed: {reason:?}"),
            _ => {}
        }
    };
    assert_eq!(
        fixture.runtime.wait(target).await.unwrap(),
        event_bus::AgentRunPhase::Error
    );
    let before = fixture
        .runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(target)
        .unwrap()
        .unwrap();
    let descriptor: crate::restore::RunRestoreDescriptor =
        serde_json::from_str(&before.config_json).unwrap();
    assert_eq!(
        descriptor.pending_reference_id.as_deref(),
        Some(original.reference_id.as_str())
    );
    drop(fixture.runtime);
    let bus = Arc::new(EventBus::new(128));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        Arc::new(ScriptedModel::new([])),
    )
    .with_run_store(
        RunStore::open(
            &storage::StorageConfig {
                db_path: fixture._dir.path().join("review.db"),
                ..Default::default()
            },
            fixture._storage.handle(),
        )
        .unwrap(),
    );
    runtime
        .continue_goal(target, "yes".into(), RunConfig::default())
        .unwrap();
    let store = runtime.shared.run_store.get().unwrap();
    let rows = store
        .review_evidence_for_lineage(&[target.to_string()])
        .unwrap();
    let row = rows.iter().find(|row| row.text == "yes").unwrap();
    let reference: crate::escalation_review::ReferenceContext =
        serde_json::from_str(row.reference_context.as_deref().unwrap()).unwrap();
    assert_eq!(reference.text, original.context);
    assert_eq!(reference.source_run_id, source.to_string());
    assert_eq!(reference.turn_end, original.turn_end);
    assert!(
        store
            .handle
            .assistant_reference(&original.reference_id)
            .unwrap()
            .unwrap()
            .consumed
    );
    runtime.stop(target, crate::StopScope::SelfOnly).unwrap();
    runtime.wait(target).await.unwrap();
}

async fn late_storage_fixture() -> (Fixture, crate::RunId, crate::runtime::PendingReference) {
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "May I read the original diagnostic output?",
        FinishReason::Stop,
    ))]));
    let mut fixture = Fixture::with_storage(model, None, false);
    let run = fixture.chat();
    fixture.completed().await;
    let original = fixture
        .runtime
        .peek_pending_reference(run)
        .unwrap()
        .unwrap();
    let config = storage::StorageConfig {
        db_path: fixture._dir.path().join("review.db"),
        ..Default::default()
    };
    fixture.runtime = fixture
        .runtime
        .with_run_store(RunStore::open(&config, fixture._storage.handle()).unwrap());
    (fixture, run, original)
}

#[tokio::test]
async fn late_storage_attachment_persists_original_before_host_reply() {
    let (fixture, run, original) = late_storage_fixture().await;
    let handle = fixture._storage.handle();
    assert!(
        handle
            .assistant_reference(&original.reference_id)
            .unwrap()
            .is_none()
    );
    fixture.runtime.send_message(run, "yes".into()).unwrap();
    let saved = handle
        .assistant_reference(&original.reference_id)
        .unwrap()
        .unwrap();
    assert!(saved.consumed);
    assert_eq!(saved.text, original.context);
    assert_eq!(saved.source_run_id, original.source_run_id);
    assert_eq!(saved.turn_end, original.turn_end);
    let evidence = fixture
        .runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .review_evidence_for_lineage(&[run.to_string()])
        .unwrap();
    let reply = evidence.iter().find(|row| row.kind == "host_chat").unwrap();
    let context: crate::escalation_review::ReferenceContext =
        serde_json::from_str(reply.reference_context.as_deref().unwrap()).unwrap();
    assert_eq!(context.text, original.context);
    assert_eq!(context.source_run_id, original.source_run_id);
    assert_eq!(context.turn_end, original.turn_end);
    assert!(
        fixture
            .runtime
            .peek_pending_reference(run)
            .unwrap()
            .is_none()
    );
    fixture.runtime.cancel(run).unwrap();
}

#[tokio::test]
async fn late_storage_attachment_never_recreates_promoted_or_consumed_references() {
    for corrupt in ["missing", "consumed"] {
        let (fixture, run, original) = late_storage_fixture().await;
        let handle = fixture._storage.handle();
        assert!(
            fixture
                .runtime
                .send_message(run, "x".repeat(256 * 1024 + 1))
                .is_err()
        );
        let saved = handle
            .assistant_reference(&original.reference_id)
            .unwrap()
            .unwrap();
        assert!(!saved.consumed);
        assert_eq!(saved.text, original.context);
        assert!(
            fixture
                .runtime
                .peek_pending_reference(run)
                .unwrap()
                .is_some()
        );
        if corrupt == "missing" {
            rusqlite::Connection::open(fixture._dir.path().join("review.db"))
                .unwrap()
                .execute(
                    "DELETE FROM assistant_references WHERE reference_id = ?1",
                    [&original.reference_id],
                )
                .unwrap();
        } else {
            handle
                .append_review_evidence_with_reference_consumption(
                    &[],
                    &[],
                    Some(&original.reference_id),
                )
                .unwrap();
        }
        assert!(fixture.runtime.send_message(run, "yes".into()).is_err());
        let after = handle.assistant_reference(&original.reference_id).unwrap();
        assert_eq!(after.is_none(), corrupt == "missing");
        assert!(after.is_none_or(|row| row.consumed));
        assert!(
            fixture
                .runtime
                .shared
                .run_store
                .get()
                .unwrap()
                .review_evidence_for_lineage(&[run.to_string()])
                .unwrap()
                .is_empty()
        );
        fixture.runtime.cancel(run).unwrap();
    }
}
