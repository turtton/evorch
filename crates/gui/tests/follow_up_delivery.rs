//! GUI queue/receipt/next-turn action through the production sink and runtime.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent};
use gui::model::commands::{ChatSubmission, CommandSink, LoopEvent, WorkbenchCommand};
use gui::model::composer::ProviderStatus;
use gui::runtime_sink::RuntimeCommandSink;
use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec, Usage};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, FixtureDeliveryAdapter, GoalSupervisor,
    OrchestrationSettings, Role, RunConfig, RuntimeError,
};
use tokio::sync::Notify;
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

#[derive(Default)]
struct GatedModel {
    calls: AtomicUsize,
    messages: Mutex<Vec<Vec<Message>>>,
    started: [Notify; 2],
    release: [Notify; 2],
}

#[async_trait::async_trait]
impl AgentModel for GatedModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "gated".into()
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        self.messages.lock().unwrap().push(messages.to_vec());
        if index < 2 {
            self.started[index].notify_one();
            self.release[index].notified().await;
        }
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: if index == 0 {
                    vec![ContentBlock::ToolUse {
                        id: "read-one".into(),
                        name: "read".into(),
                        input: serde_json::json!({"path":"not-present"}),
                    }]
                } else {
                    vec![ContentBlock::Text {
                        text: "answer".into(),
                    }]
                },
            },
            usage: Usage::default(),
            finish_reason: if index == 0 {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            },
        })
    }
}

#[test]
fn gui_delivery_banner_and_action_track_chat_and_goal_receipts() {
    for goal in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let bus = Arc::new(EventBus::new(256));
        let mut receiver = bus.subscribe();
        let model = Arc::new(GatedModel::default());
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus.clone())),
            model.clone(),
        );
        let supervisor = rt.block_on(async {
            GoalSupervisor::spawn(
                runtime.clone(),
                bus,
                Arc::new(FixtureDeliveryAdapter::default()),
                OrchestrationSettings::default(),
            )
        });
        let mut sink = RuntimeCommandSink::new(runtime.clone(), rt.handle().clone(), supervisor);
        let run = rt.block_on(async {
            if goal {
                runtime.delegate_background(
                    Role::Worker,
                    "original task".into(),
                    RunConfig::default(),
                )
            } else {
                let events = sink.submit(WorkbenchCommand::SendChat(ChatSubmission {
                    fork_seed: None,
                    thread_id: "thread-1".into(),
                    text: "original task".into(),
                    images: vec![],
                    composer_role: gui::model::composer::ComposerRole::Worker,
                    model_preference: None,
                }));
                let [LoopEvent::ChatAccepted { run_id, .. }] = events.as_slice() else {
                    panic!("chat accepted: {events:?}");
                };
                runtime::RunId::new(run_id.strip_prefix("run-").unwrap().parse().unwrap())
            }
        });
        // Both routes must resolve the same root, never a child or a fresh chat.
        if goal {
            sink.bind_goal_context("thread-1", "project", &run.to_string());
        }
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), model.started[0].notified())
                .await
                .unwrap();
        });
        let mut sidebar = SidebarState::default();
        let project = ProjectId::new("project");
        sidebar
            .add_project(project.clone(), "project", directory.path())
            .unwrap();
        sidebar.select_project(&project).unwrap();
        for name in ["thread-1", "thread-2"] {
            sidebar
                .create_thread(ThreadId::new(name), project.clone(), name)
                .unwrap();
        }
        sidebar.switch_thread(&ThreadId::new("thread-1")).unwrap();
        let mut state = WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
            .unwrap()
            .with_sidebar(sidebar)
            .with_provider_status(ProviderStatus::Configured)
            .with_command_sink(Box::new(sink));
        state.apply_loop_event(gui::model::commands::LoopEvent::ChatAccepted {
            thread_id: "thread-1".into(),
            run_id: run.to_string(),
        });
        let startup = rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                let mut events = Vec::new();
                loop {
                    let event = receiver.recv().await.unwrap();
                    let running = matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to: AgentRunPhase::Running, .. }) if run_id == &run.to_string());
                    events.push(event);
                    if running { return events; }
                }
            }).await.unwrap()
        });
        state.apply_events(startup);
        let mut h = HeadlessWorkbench::new(state, [1400.0, 1000.0]);
        h.state_mut().composer_mut().input = "queued follow-up".into();
        h.run();
        h.click_label("Queue");
        h.run();
        assert!(h.has_label("配送待ち 1件 — agent には未反映（通常は回答完了後に配送）"));
        assert_eq!(runtime.follow_up_status(run).unwrap().pending, 1);
        h.state_mut()
            .switch_thread(ThreadId::new("thread-2"))
            .unwrap();
        h.run();
        assert!(!h.has_label("次のターンで届ける"));
        h.state_mut()
            .switch_thread(ThreadId::new("thread-1"))
            .unwrap();
        h.run();
        h.click_label("次のターンで届ける");
        h.run();
        assert!(runtime.follow_up_status(run).unwrap().next_turn_requested);
        assert!(h.has_label("配送待ち 1件 — 現在の応答・ツール完了後、次のターンで反映"));
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        model.release[0].notify_one();
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), model.started[1].notified())
                .await
                .unwrap();
        });
        h.run();
        assert!(!h.has_label("次のターンで届ける"));
        assert_eq!(runtime.follow_up_status(run).unwrap().pending, 0);
        let messages = model.messages.lock().unwrap();
        assert!(
            messages[1]
                .iter()
                .any(|m| m.content.contains(&ContentBlock::Text {
                    text: "queued follow-up".into()
                }))
        );
        assert!(messages[1].iter().any(|m| m.content.iter().any(|b| matches!(b, ContentBlock::ToolResult { tool_call_id, .. } if tool_call_id == "read-one"))));
        drop(messages);
        model.release[1].notify_one();
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let event = receiver.recv().await.unwrap();
                    if matches!(
                        event.kind,
                        EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                            to: AgentRunPhase::Done | AgentRunPhase::Waiting,
                            ..
                        })
                    ) {
                        break;
                    }
                }
            })
            .await
            .unwrap();
            runtime.cancel(run).unwrap();
            runtime.wait(run).await.unwrap();
        });
        assert_eq!(runtime.list_agents().len(), 1);
    }
}
