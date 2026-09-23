mod support;

use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent};
use providers::FinishReason;
use runtime::{AgentModel, AgentRuntime, Role, RunConfig, RunId, RunStore};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use storage::{Storage, StorageConfig};
use support::{ScriptedModel, text_response, tool_response};
use tools::ToolExecutor;

fn ask() -> providers::ChatResponse {
    tool_response(
        "ask",
        "ask_user",
        serde_json::json!({"title":"Required scope"}),
    )
}
fn escalate() -> providers::ChatResponse {
    tool_response(
        "escalate",
        "escalate",
        serde_json::json!({
            "original_request":"Complete work", "escalation_reason":"Need coordination"
        }),
    )
}
fn setup(
    model: Arc<dyn AgentModel>,
) -> (
    tempfile::TempDir,
    Storage,
    StorageConfig,
    AgentRuntime,
    event_bus::EventReceiver,
) {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("questions.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(256));
    let events = bus.subscribe();
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model)
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    (dir, storage, config, runtime, events)
}
async fn handoff(events: &mut event_bus::EventReceiver, source: RunId) -> RunId {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let EventKind::Lifecycle(LifecycleEvent::EscalationRequested {
                source_run_id,
                new_run_id,
                ..
            }) = events.recv().await.unwrap().kind
                && source_run_id == source.to_string()
            {
                return RunId::new(new_run_id.strip_prefix("run-").unwrap().parse().unwrap());
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn escalated_root_cannot_finish_until_inherited_required_answer_arrives() {
    let model = Arc::new(ScriptedModel::new([
        Ok(ask()),
        Ok(escalate()),
        Ok(tool_response(
            "finish",
            "finish",
            serde_json::json!({"result":"premature"}),
        )),
        Ok(text_response("Waiting for scope", FinishReason::Stop)),
        Ok(text_response("Applied scope A", FinishReason::Stop)),
    ]));
    let (_dir, _storage, _config, runtime, mut events) = setup(model.clone());
    let source = runtime.delegate_background(Role::Worker, "work".into(), RunConfig::default());
    let recipient = handoff(&mut events, source).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to: AgentRunPhase::Waiting,
                ..
            }) = events.recv().await.unwrap().kind
                && run_id == recipient.to_string()
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        runtime.inspect_agent(recipient).unwrap().phase,
        AgentRunPhase::Waiting
    );
    let question = runtime.user_answers(recipient).unwrap().remove(0);
    assert_eq!(
        question.run_id,
        source.to_string(),
        "retain requester provenance"
    );
    assert!(runtime.has_active_question_recipient(&question.id).unwrap());
    let observed = model.observed().await;
    assert!(
        serde_json::to_string(observed.last().unwrap())
            .unwrap()
            .contains("Required user answers are pending")
    );
    runtime.answer_user_question(&question.id, "A").unwrap();
    assert_eq!(runtime.wait(recipient).await.unwrap(), AgentRunPhase::Done);
    let observed = model.observed().await;
    let last = serde_json::to_string(observed.last().unwrap()).unwrap();
    assert!(last.contains("Answer: A"));
    assert_eq!(last.matches("[user-answer id=").count(), 1);
}

struct BeforeEscalation {
    runtime: Mutex<Option<AgentRuntime>>,
    drop_links: Mutex<Option<std::path::PathBuf>>,
    calls: AtomicUsize,
    script: ScriptedModel,
}
#[async_trait::async_trait]
impl AgentModel for BeforeEscalation {
    async fn complete(
        &self,
        invocation: &runtime::AgentInvocationContext,
        role: Role,
        messages: &[providers::Message],
        tools: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
            if let Some(path) = self.drop_links.lock().unwrap().take() {
                rusqlite::Connection::open(path)
                    .unwrap()
                    .execute_batch("DROP TABLE user_question_links")
                    .unwrap();
            } else {
                let runtime = self.runtime.lock().unwrap().as_ref().unwrap().clone();
                let source = RunId::new(
                    invocation
                        .run_id
                        .strip_prefix("run-")
                        .unwrap()
                        .parse()
                        .unwrap(),
                );
                let question = runtime.user_answers(source).unwrap().remove(0);
                runtime
                    .answer_user_question(&question.id, "A before escalation")
                    .unwrap();
            }
        }
        self.script
            .complete(invocation, role, messages, tools)
            .await
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "escalation-question-fixture".into()
    }
}
fn hook_model() -> Arc<BeforeEscalation> {
    Arc::new(BeforeEscalation {
        runtime: Mutex::new(None),
        drop_links: Mutex::new(None),
        calls: AtomicUsize::new(0),
        script: ScriptedModel::new([
            Ok(ask()),
            Ok(escalate()),
            Ok(text_response("Applied answer", FinishReason::Stop)),
        ]),
    })
}

#[tokio::test]
async fn answer_saved_before_escalation_is_injected_into_the_new_roots_fresh_context() {
    let model = hook_model();
    let (_dir, _storage, _config, runtime, mut events) = setup(model.clone());
    *model.runtime.lock().unwrap() = Some(runtime.clone());
    let source = runtime.delegate_background(Role::Worker, "work".into(), RunConfig::default());
    let recipient = handoff(&mut events, source).await;
    assert_eq!(runtime.wait(recipient).await.unwrap(), AgentRunPhase::Done);
    let observed = model.script.observed().await;
    assert_eq!(observed.len(), 3);
    let last = serde_json::to_string(observed.last().unwrap()).unwrap();
    assert!(last.contains("A before escalation"));
    assert_eq!(last.matches("[user-answer id=").count(), 1);
    *model.runtime.lock().unwrap() = None;
}

#[tokio::test]
async fn failed_question_inheritance_reports_error_without_spawning_an_escalated_root() {
    let model = hook_model();
    let (_dir, _storage, config, runtime, mut events) = setup(model.clone());
    *model.drop_links.lock().unwrap() = Some(config.db_path);
    let source = runtime.delegate_background(Role::Worker, "work".into(), RunConfig::default());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::Diagnostic(event) if event.code == "EscalationHandoffFailed" => {
                    assert_eq!(event.run_id.as_deref(), Some(source.to_string().as_str()));
                    assert!(event.detail.contains("question inheritance failed"));
                    break;
                }
                EventKind::Lifecycle(LifecycleEvent::EscalationRequested { .. }) => {
                    panic!("failed inheritance must not start a root")
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(runtime.list_agents().len(), 1);
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
}
