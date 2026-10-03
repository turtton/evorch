use std::sync::{Arc, Mutex};
use std::time::Duration;

use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent, ToolEvent};
use gui::{
    app::WorkbenchState,
    fixture::DemoSource,
    headless::HeadlessWorkbench,
    model::{composer::ProviderStatus, transcript::TranscriptEntry},
    runtime_sink::RuntimeCommandSink,
};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, FixtureDeliveryAdapter, GoalSupervisor,
    OrchestrationSettings, Role, RunId, RunStore, RuntimeError,
};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

#[derive(Default)]
struct QuestionHandoffModel(Mutex<Vec<Vec<Message>>>);
#[async_trait::async_trait]
impl AgentModel for QuestionHandoffModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let mut requests = self.0.lock().unwrap();
        requests.push(messages.to_vec());
        let (name, input) = match requests.len() {
            1 => (
                "ask_user",
                serde_json::json!({"title":"Which output format?", "options":["JSON"]}),
            ),
            2 => (
                "escalate",
                serde_json::json!({"original_request":"Implement feature", "escalation_reason":"Need coordination"}),
            ),
            3 => ("finish", serde_json::json!({"result":"premature"})),
            4 => {
                return Ok(ChatResponse {
                    message: Message {
                        role: providers::Role::Assistant,
                        content: vec![ContentBlock::Text {
                            text: "Waiting for scope".into(),
                        }],
                    },
                    usage: Default::default(),
                    finish_reason: FinishReason::Stop,
                });
            }
            5 => ("finish", serde_json::json!({"result":"Applied JSON scope"})),
            count => panic!("unexpected request {count}"),
        };
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: format!("call-{}", requests.len()),
                    name: name.into(),
                    input,
                }],
            },
            usage: Default::default(),
            finish_reason: FinishReason::ToolUse,
        })
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "fixture".into()
    }
}

#[test]
fn inherited_question_is_visible_after_restart_and_answered_in_destination_before_finish() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("questions.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let bus = Arc::new(EventBus::new(512));
    let mut receiver = bus.subscribe();
    let owner = Arc::new(
        runtime::ownership::OwnerHost::open(
            &dir.path().join("owners"),
            Default::default(),
            bus.clone(),
        )
        .unwrap(),
    );
    let model = Arc::new(QuestionHandoffModel::default());
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus.clone())),
        model.clone(),
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
    for thread in ["source", "unrelated"] {
        sidebar
            .create_thread(ThreadId::new(thread), project.clone(), thread)
            .unwrap();
    }
    sidebar.switch_thread(&ThreadId::new("source")).unwrap();
    let mut state = WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
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
    let mut updated = None;
    let root = rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut destination = None;
            loop {
                let event = receiver.recv().await.unwrap();
                storage.handle().append_event(Some("gui"), &event).unwrap();
                state.apply_events([event.clone()]);
                match event.kind {
                    EventKind::Tool(ToolEvent::UserQuestionUpdated { question })
                        if !question.recipient_run_ids.is_empty() =>
                    {
                        updated = Some(question)
                    }
                    EventKind::Lifecycle(LifecycleEvent::EscalationRequested {
                        new_run_id,
                        ..
                    }) => destination = Some(new_run_id),
                    EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                        run_id,
                        to: AgentRunPhase::Waiting,
                        ..
                    }) if destination.as_ref() == Some(&run_id) => break run_id,
                    _ => {}
                }
            }
        })
        .await
        .unwrap()
    });
    let question = updated.expect("routing update published after durable inheritance");
    assert_eq!(question.recipient_run_ids, std::slice::from_ref(&root));
    assert_eq!(question.root_name, "chat:Worker:source");
    assert_eq!(question.run_id, question.root_run_id);
    let destination = ThreadId::new(format!("escalation-{root}"));
    let notice = state
        .transcript()
        .entries()
        .iter()
        .find_map(|entry| match entry {
            TranscriptEntry::Notice { text } if text.contains("引き継ぎ済み") => {
                Some(text.clone())
            }
            _ => None,
        })
        .expect("source thread explains where the existing question went");
    assert!(notice.contains(&question.id));
    assert!(notice.contains(&destination.to_string()));

    // Reopen durable reads and replay with the same sidebar; no question cloning.
    let mut replay = WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(state.sidebar().clone());
    replay
        .restore_history(&storage::Database::open(&config).unwrap())
        .unwrap();
    assert_eq!(
        replay.pending_user_questions().cloned().collect::<Vec<_>>(),
        std::slice::from_ref(&question)
    );
    let mut restored = HeadlessWorkbench::new(replay, [1400.0, 1000.0]);
    restored.run();
    assert!(restored.has_label("Which output format?"));
    assert!(restored.has_label(&notice));
    restored
        .state_mut()
        .switch_thread(destination.clone())
        .unwrap();
    restored.run();
    assert!(restored.has_label("Which output format?"));
    restored
        .state_mut()
        .switch_thread(ThreadId::new("unrelated"))
        .unwrap();
    restored.run();
    assert!(!restored.has_label("Which output format?"));

    state.switch_thread(destination).unwrap();
    assert!(state.thread_writable());
    let mut ui = HeadlessWorkbench::new(state, [1400.0, 1000.0]);
    ui.run();
    assert!(ui.has_label("Which output format?"));
    ui.click_label("JSON");
    ui.run();
    ui.click_label("回答を送信");
    ui.run();
    let run = RunId::new(root.strip_prefix("run-").unwrap().parse().unwrap());
    assert_eq!(
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), runtime.wait(run))
                .await
                .unwrap()
                .unwrap()
        }),
        AgentRunPhase::Done
    );
    assert_eq!(runtime.user_answers(run).unwrap().len(), 1);
    let answered = runtime.user_question(&question.id).unwrap().unwrap();
    assert_eq!(answered.answer.as_deref(), Some("JSON"));
    assert_eq!(answered.run_id, question.run_id);
    assert_eq!(answered.root_name, question.root_name);
    assert_eq!(ui.state().pending_user_questions().count(), 0);
    let requests = model.0.lock().unwrap();
    let initial = serde_json::to_string(&requests[2]).unwrap();
    assert!(initial.contains(&question.id));
    assert!(initial.contains("新 ID で再 ask せず"));
    assert!(
        serde_json::to_string(&requests[3])
            .unwrap()
            .contains("Required user answers are pending")
    );
    let last = serde_json::to_string(requests.last().unwrap()).unwrap();
    assert!(last.contains("Answer: JSON"));
    assert_eq!(last.matches("[user-answer id=").count(), 1);
}
