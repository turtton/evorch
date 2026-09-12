use std::sync::Arc;

use async_trait::async_trait;
use event_bus::{AgentRunPhase, EventBus};
use gui::app::WorkbenchState;
use gui::headless::HeadlessWorkbench;
use gui::model::composer::ProviderStatus;
use gui::model::transcript::TranscriptEntry;
use gui::runtime_sink::RuntimeCommandSink;
use providers::{ChatResponse, Message, ToolSpec};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, FixtureDeliveryAdapter, GoalSupervisor,
    OrchestrationSettings, Role, RuntimeError,
};
use tools::ToolExecutor;
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

struct PendingModel;

#[async_trait]
impl AgentModel for PendingModel {
    async fn complete(
        &self,
        _invocation: &AgentInvocationContext,
        _role: Role,
        _messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        std::future::pending().await
    }

    fn selected_model(&self, _role: Role) -> String {
        "test-background".into()
    }
}

fn workbench(root: &std::path::Path) -> (HeadlessWorkbench<AgentRuntime>, tokio::runtime::Runtime) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let bus = Arc::new(EventBus::new(64));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus.clone())),
        Arc::new(PendingModel),
    );
    let supervisor = rt.block_on(async {
        GoalSupervisor::spawn(
            runtime.clone(),
            bus,
            Arc::new(FixtureDeliveryAdapter::default()),
            OrchestrationSettings::default(),
        )
    });
    let sink = RuntimeCommandSink::new(runtime.clone(), rt.handle().clone(), supervisor);
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("demo");
    sidebar
        .add_project(project_id.clone(), "demo", root)
        .expect("project added");
    sidebar
        .select_project(&project_id)
        .expect("project selected");
    sidebar
        .create_thread(ThreadId::new("thread-1"), project_id, "thread-1")
        .expect("thread created");
    sidebar
        .switch_thread(&ThreadId::new("thread-1"))
        .expect("thread selected");
    let state = WorkbenchState::new(runtime, &UiSettings::default())
        .expect("default state builds")
        .with_sidebar(sidebar)
        .with_provider_status(ProviderStatus::Configured)
        .with_command_sink(Box::new(sink));
    (HeadlessWorkbench::new(state, [1200.0, 900.0]), rt)
}

#[test]
fn background_run_submission_keeps_focus_and_opens_no_pane() {
    // Given: the same selected project/thread as composer_dispatch_headless.
    let temp = tempfile::tempdir().expect("temp dir");
    let (mut harness, _rt) = workbench(temp.path());
    harness.run();
    let focus = harness.state().focus().clone();
    let panels: Vec<_> = harness
        .state()
        .dock()
        .iter_all_tabs()
        .map(|(_, panel)| panel.clone())
        .collect();
    // When: the normal composer submission path launches background work.
    harness.state_mut().composer_mut().input = "/run do the thing".into();
    harness.state_mut().submit_composer();
    harness.run();
    // Then: only a launch notice is added; the conversation stays unbound.
    assert_eq!(harness.state().focus(), &focus);
    assert_eq!(harness.state().dock().iter_all_tabs().count(), panels.len());
    assert!(
        harness.state().dock().iter_all_tabs().all(|(_, panel)| {
            !panel.to_string().starts_with("agent-") || panels.contains(panel)
        })
    );
    assert!(harness.state().composer().input.is_empty());
    assert!(matches!(
        harness.state().transcripts().thread().entries(),
        [TranscriptEntry::Notice { text }] if text.contains("run-")
    ));
    assert!(harness.state().sidebar().threads[0].run_ids.is_empty());
    assert!(harness.state().issued().is_empty());
}

#[test]
fn background_run_is_fire_and_forget() {
    // Given: the runtime cannot progress until explicitly driven by this test.
    let temp = tempfile::tempdir().expect("temp dir");
    let (mut harness, _rt) = workbench(temp.path());
    // When: submit returns synchronously without driving the runtime.
    harness.state_mut().composer_mut().input = "/run do the thing".into();
    harness.state_mut().submit_composer();
    harness.run();
    // Then: the real runtime source contains a still-unfinished worker.
    let run = harness
        .state()
        .tasks()
        .inspect(runtime::RunId::new(1))
        .expect("background run registered in runtime source");
    assert!(matches!(
        run.phase,
        AgentRunPhase::Pending | AgentRunPhase::Running
    ));
    assert_eq!(run.role_name, "Worker");
}
