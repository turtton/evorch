use std::sync::{Arc, Mutex};
use std::time::Duration;

use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent};
use gui::{
    app::WorkbenchState,
    fixture::DemoSource,
    model::{composer::ProviderStatus, transcript::TranscriptEntry},
    runtime_sink::RuntimeCommandSink,
};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, FixtureDeliveryAdapter, GoalSupervisor,
    OrchestrationSettings, Role, RunStore, RuntimeError,
};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

type Requests = Arc<Mutex<Vec<(Role, Vec<Message>)>>>;
struct EscalatingModel(Requests);
#[async_trait::async_trait]
impl AgentModel for EscalatingModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.0.lock().unwrap().push((role, messages.to_vec()));
        let (content, finish_reason) = if role == Role::Worker && self.0.lock().unwrap().len() == 1
        {
            (
                ContentBlock::ToolUse {
                    id: "escalate-test".into(),
                    name: "escalate".into(),
                    input: serde_json::json!({
                        "original_request": "Implement feature", "escalation_reason": "Need coordination",
                        "findings": [], "files_touched": [], "blockers": ["Need review"],
                        "workspace_state": "clean", "suggested_next": "Review design"
                    }),
                },
                FinishReason::ToolUse,
            )
        } else if self.0.lock().unwrap().len() == 2 {
            (
                ContentBlock::ToolUse {
                    id: "finish-test".into(),
                    name: "finish".into(),
                    input: serde_json::json!({"result": "orchestrator answer"}),
                },
                FinishReason::ToolUse,
            )
        } else {
            (
                ContentBlock::Text {
                    text: "orchestrator answer".into(),
                },
                FinishReason::Stop,
            )
        };
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: vec![content],
            },
            usage: Default::default(),
            finish_reason,
        })
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "fixture".into()
    }
}

#[test]
fn child_composer_continues_same_orchestrator_after_completion_and_while_alive() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("runs.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let bus = Arc::new(EventBus::new(512));
    let owner = Arc::new(
        runtime::ownership::OwnerHost::open(
            &dir.path().join("owners"),
            Default::default(),
            bus.clone(),
        )
        .unwrap(),
    );
    let mut receiver = bus.subscribe();
    let requests = Requests::default();
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus.clone())),
        Arc::new(EscalatingModel(requests.clone())),
    )
    .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let supervisor = rt.block_on(async {
        GoalSupervisor::spawn(
            runtime.clone(),
            bus,
            Arc::new(FixtureDeliveryAdapter::default()),
            OrchestrationSettings {
                max_continuations: 0,
                ..Default::default()
            },
        )
    });
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", dir.path())
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("parent"), project, "Worker task")
        .unwrap();
    sidebar.switch_thread(&ThreadId::new("parent")).unwrap();
    let mut state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_ownership(owner.clone())
        .with_provider_status(ProviderStatus::Configured)
        .with_command_sink(Box::new(
            RuntimeCommandSink::new(runtime.clone(), rt.handle().clone(), supervisor)
                .with_ownership(owner),
        ));
    state.composer_mut().input = "Implement feature".into();
    state.submit_composer();
    let root = rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut root = None;
            let mut final_seen = false;
            loop {
                let event = receiver.recv().await.unwrap();
                state.apply_events([event.clone()]);
                if let EventKind::Lifecycle(LifecycleEvent::EscalationRequested {
                    new_run_id,
                    ..
                }) = &event.kind
                {
                    root = Some(new_run_id.clone());
                }
                if let EventKind::Message(event_bus::MessageEvent::FinalResultPublished {
                    run_id, text,
                }) = &event.kind
                {
                    assert_eq!(root.as_ref(), Some(run_id));
                    assert_eq!(text, "orchestrator answer");
                    assert!(state.transcripts().run(run_id).unwrap().entries().iter().any(|entry|
                        matches!(entry, TranscriptEntry::Message { text, .. } if text == "orchestrator answer")));
                    final_seen = true;
                }
                if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                    run_id,
                    to: AgentRunPhase::Done,
                    ..
                }) = &event.kind
                    && root.as_ref() == Some(run_id)
                {
                    assert!(final_seen, "canonical result is visible before Done");
                    break run_id.clone();
                }
            }
        })
        .await
        .unwrap()
    });
    assert_eq!(runtime.list_agents().len(), 2);
    let initial = requests.lock().unwrap()[1].1.clone();
    let child = ThreadId::new(format!("escalation-{root}"));
    state.switch_thread(child.clone()).unwrap();
    assert!(
        state.thread_writable(),
        "the source owner can answer and cancel in its child thread"
    );
    assert!(
        state
            .transcript()
            .entries()
            .iter()
            .any(|entry| matches!(entry,
        TranscriptEntry::Message { text, .. } if text == "orchestrator answer"))
    );
    for text in ["Continue the same design", "Review it again"] {
        state.composer_mut().input = text.into();
        state.submit_composer();
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let event = receiver.recv().await.unwrap();
                    state.apply_events([event.clone()]);
                    if matches!(&event.kind,
                        EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to: AgentRunPhase::Waiting, .. }) if run_id == &root) {
                        break;
                    }
                }
            }).await.unwrap();
        });
    }
    assert_eq!(
        runtime.list_agents().len(),
        2,
        "both continuations reuse the orchestrator root"
    );
    assert_eq!(
        state
            .sidebar()
            .threads
            .iter()
            .find(|thread| thread.id == child)
            .unwrap()
            .run_ids,
        [root]
    );
    let recorded_requests = requests.clone();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert!(requests[2].1.starts_with(&initial));
    assert!(requests[3].1.starts_with(&requests[2].1));
    assert!(
        requests[1..]
            .iter()
            .all(|(role, _)| *role == Role::Orchestrator)
    );
    let original = requests[0].1.clone();
    drop(requests);

    // A prior-version source checkpoint like run-98: escalation finished, but
    // an asynchronous shell outcome was never collected. The original thread
    // must remain a usable conversation without switching to the Orchestrator.
    let source = state
        .sidebar()
        .threads
        .iter()
        .find(|t| t.id == ThreadId::new("parent"))
        .unwrap()
        .run_ids[0]
        .clone();
    let mut record = storage::Database::open(&config)
        .unwrap()
        .run_context(&source)
        .unwrap()
        .unwrap();
    let mut descriptor: serde_json::Value = serde_json::from_str(&record.config_json).unwrap();
    descriptor["restorable"] = serde_json::json!(false);
    descriptor["non_restorable_reason"] = serde_json::json!(
        "unresolved_tool_calls: inspect actual effects before starting a new run"
    );
    descriptor["interrupted_tool_calls"] = serde_json::json!([{
        "call_id":"unobserved-shell-jobs", "tool_name":"shell", "result_observed":false, "may_have_side_effects":true
    }]);
    record.config_json = descriptor.to_string();
    record.restorable = false;
    storage.handle().upsert_run_context(&record).unwrap();
    state.switch_thread(ThreadId::new("parent")).unwrap();
    state.composer_mut().input = "状況を教えて".into();
    state.submit_composer();
    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = receiver.recv().await.unwrap();
                state.apply_events([event.clone()]);
                if matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {run_id, to:AgentRunPhase::Waiting, ..}) if run_id == &source) {
                    break;
                }
            }
        }).await.unwrap();
    });
    let observed = recorded_requests.lock().unwrap();
    assert_eq!(observed.len(), 5);
    assert_eq!(observed[4].0, Role::Worker);
    assert!(observed[4].1.starts_with(&original));
    let text = serde_json::to_string(&observed[4].1).unwrap();
    assert!(text.contains("ToolExecutionOutcomeUnknown"));
    assert!(text.contains("状況を教えて"));
    assert_eq!(
        runtime.list_agents().len(),
        2,
        "no replacement thread or run is required"
    );
}
