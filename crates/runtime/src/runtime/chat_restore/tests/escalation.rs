use super::*;
use providers::{ContentBlock, Message, ToolResultContent, ToolSpec};
use serde_json::{Value, json};
use std::collections::VecDeque;
use tokio::sync::Notify;

enum Step {
    Tool(&'static str, Value),
    Stop,
    Hold(Arc<Notify>),
}

type Request = (Vec<Message>, Vec<ToolSpec>);

struct ScriptedModel {
    steps: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<Request>>,
}

impl ScriptedModel {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into()),
            requests: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl AgentModel for ScriptedModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }

    async fn complete(
        &self,
        context: &crate::AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        let index = {
            let mut requests = self.requests.lock().unwrap();
            requests.push((messages.to_vec(), tools.to_vec()));
            requests.len()
        };
        let step = self.steps.lock().unwrap().pop_front().unwrap();
        match step {
            Step::Tool(name, input) => Ok(providers::ChatResponse {
                message: Message {
                    role: providers::Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: format!("call-{index}"),
                        name: name.into(),
                        input,
                    }],
                },
                finish_reason: providers::FinishReason::ToolUse,
                usage: Default::default(),
            }),
            Step::Stop => {
                CompletingModel
                    .complete(context, role, messages, tools)
                    .await
            }
            Step::Hold(entered) => {
                // The next request proves that all preceding tool results arrived.
                // Cancellation releases this request; no wall-clock wait is needed.
                entered.notify_one();
                std::future::pending().await
            }
        }
    }
}

#[tokio::test]
async fn escalation_without_goal_restores_thread_tools_after_restart() {
    restored_thread_tools(false).await;
}

#[tokio::test]
async fn escalation_without_goal_resumes_thread_tools_after_restart() {
    restored_thread_tools(true).await;
}

async fn restored_thread_tools(resume_first: bool) {
    let initial_model = ScriptedModel::new(vec![
        Step::Tool(
            "escalate",
            json!({"original_request":"Research the options","escalation_reason":"Needs orchestration"}),
        ),
        Step::Stop,
    ]);
    let fixture = Fixture::with_model(initial_model.clone());
    let mut events = fixture.runtime.shared.bus.subscribe();
    let source = fixture
        .runtime
        .delegate_chat(
            "source",
            Role::Worker,
            "Research the options".into(),
            RunConfig {
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    let escalated = loop {
        if let event_bus::EventKind::Lifecycle(LifecycleEvent::EscalationRequested {
            source_run_id,
            new_run_id,
            ..
        }) = events.recv().await.unwrap().kind
        {
            assert_eq!(source_run_id, source.to_string());
            break new_run_id.parse::<RunId>().unwrap();
        }
    };
    assert_eq!(
        fixture.runtime.wait(source).await.unwrap(),
        AgentRunPhase::Done
    );
    assert_eq!(
        fixture.runtime.wait(escalated).await.unwrap(),
        AgentRunPhase::Done
    );
    let thread = event_bus::escalation_thread_id(&escalated.to_string());
    assert!(fixture.runtime.thread_goal(&thread).is_none());
    let record = fixture
        .runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(escalated)
        .unwrap()
        .unwrap();
    let descriptor: RunRestoreDescriptor = serde_json::from_str(&record.config_json).unwrap();
    assert!(
        descriptor.pending_escalation.is_none(),
        "not the initial seed"
    );
    assert_eq!(descriptor.thread_id.as_deref(), Some(thread.as_str()));
    let saved_messages: Vec<Message> = serde_json::from_str(&record.messages_json).unwrap();
    assert!(
        saved_messages
            .iter()
            .any(|message| { message.role == providers::Role::Assistant })
    );
    let original_request = initial_model.requests.lock().unwrap()[1].clone();
    drop(fixture.runtime);

    let artifacts = Arc::new(
        crate::artifacts::ArtifactStore::new(fixture._dir.path().join("artifacts")).unwrap(),
    );
    std::fs::write(fixture._dir.path().join("mock.html"), "<p>Options</p>").unwrap();
    let artifact = artifacts
        .capture(
            fixture._dir.path(),
            None,
            std::path::Path::new("mock.html"),
            crate::artifacts::CaptureRequest {
                title: "Options",
                caption: None,
                run_id: source.to_string(),
                root_run_id: source.to_string(),
                thread_id: Some(thread.clone()),
            },
        )
        .unwrap();
    // A different root's artifact requires thread identity, not the same-root fallback.
    let entered = Arc::new(Notify::new());
    let model = ScriptedModel::new(vec![
        Step::Tool(
            "create_goal",
            json!({"objective":"Research the options","criteria":["Evidence for each option"]}),
        ),
        Step::Tool("get_goal", json!({})),
        Step::Tool(
            "todo_write",
            json!({"items":[{"content":"Gather evidence","status":"pending"}]}),
        ),
        Step::Tool("present", json!({"artifact_ids":[artifact.artifact_id]})),
        Step::Hold(entered.clone()),
    ]);
    let bus = Arc::new(EventBus::new(128));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(
            crate::RunStore::open(
                &storage::StorageConfig {
                    db_path: fixture._dir.path().join("goal.sqlite3"),
                    ..Default::default()
                },
                fixture.storage.handle(),
            )
            .unwrap(),
        )
        .with_artifact_store(artifacts);
    assert!(runtime.goal_thread(escalated).is_none());
    // As the GUI does on restart, restore the host declaration before starting the run.
    let read_dir = fixture._dir.path().join("references");
    std::fs::create_dir(&read_dir).unwrap();
    runtime.set_thread_read_only_directories(&thread, vec![read_dir.clone()]);
    let authority = RunConfig {
        conversation: true,
        ..Default::default()
    };
    if resume_first {
        runtime.resume_chat(escalated, authority.clone()).unwrap();
        assert_eq!(
            runtime.goal_thread(escalated).as_deref(),
            Some(thread.as_str())
        );
    }
    runtime
        .continue_goal(
            escalated,
            "Continue researching until the criteria are met".into(),
            authority,
        )
        .unwrap();
    entered.notified().await;
    runtime.stop(escalated, StopScope::SelfOnly).unwrap();
    assert_eq!(
        runtime.wait(escalated).await.unwrap(),
        AgentRunPhase::Stopped
    );

    let requests = model.requests.lock().unwrap();
    let results: Vec<_> = requests
        .last()
        .unwrap()
        .0
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } => {
                let text = content
                    .iter()
                    .filter_map(|block| match block {
                        ToolResultContent::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                Some((tool_call_id, *is_error, text))
            }
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 4);
    assert!(results.iter().all(|(_, error, _)| !error), "{results:?}");
    let goal: Value = serde_json::from_str(&results[1].2).unwrap();
    assert_eq!(goal["thread_id"], thread);
    assert_eq!(goal["root_run_id"], escalated.to_string());
    assert_eq!(runtime.thread_todo(&thread).unwrap().items.len(), 1);
    assert_eq!(
        runtime.goal_thread(escalated).as_deref(),
        Some(thread.as_str())
    );
    assert_eq!(
        runtime.thread_of_live_run(escalated).as_deref(),
        Some(thread.as_str())
    );
    let resolved = runtime.thread_of_run(escalated).unwrap();
    assert_eq!(
        runtime.mountable_thread_read_only_directories(
            &resolved,
            &fixture._dir.path().join("workspace"),
            None,
        ),
        vec![read_dir.canonicalize().unwrap()]
    );
    // Rebinding must not rewrite system instructions, tool schemas or already-sent
    // history. New human input and tool rounds only append history.
    assert_eq!(requests[0].1, original_request.1);
    assert_eq!(&requests[0].0[..saved_messages.len()], &saved_messages);
    for pair in requests.windows(2) {
        assert_eq!(pair[0].1, pair[1].1);
        assert_eq!(&pair[1].0[..pair[0].0.len()], &pair[0].0);
    }
}
