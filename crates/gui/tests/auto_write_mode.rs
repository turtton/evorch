use event_bus::EventBus;
use gui::model::{
    commands::{ChatSubmission, CommandSink, LoopEvent, WorkbenchCommand},
    composer::ProviderStatus,
    transcript::TranscriptEntry,
};
use gui::{app::WorkbenchState, fixture::DemoSource};
use runtime::ownership::{OwnerHost, OwnerPermit, RegistryError};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Observed {
    commands: Vec<WorkbenchCommand>,
    permits: Vec<OwnerPermit>,
}
struct Sink(Arc<Mutex<Observed>>);
impl CommandSink for Sink {
    fn submit(&mut self, command: WorkbenchCommand) -> Vec<LoopEvent> {
        self.0.lock().unwrap().commands.push(command);
        Vec::new()
    }
    fn submit_chat_with_permit(
        &mut self,
        chat: ChatSubmission,
        permit: OwnerPermit,
    ) -> Vec<LoopEvent> {
        permit.validate_generation().unwrap();
        self.0.lock().unwrap().permits.push(permit);
        self.submit(WorkbenchCommand::SendChat(chat))
    }
}
fn host(dir: &tempfile::TempDir) -> Arc<OwnerHost> {
    Arc::new(OwnerHost::open(dir.path(), Default::default(), Arc::new(EventBus::new(64))).unwrap())
}
fn state(host: Arc<OwnerHost>) -> (WorkbenchState<DemoSource>, Arc<Mutex<Observed>>) {
    let mut sidebar = workspace_ui::SidebarState::default();
    let project = workspace_ui::ProjectId::new("project");
    let thread = workspace_ui::ThreadId::new("thread");
    sidebar
        .add_project(project.clone(), "project", &std::env::temp_dir())
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(thread.clone(), project, "chat")
        .unwrap();
    sidebar.switch_thread(&thread).unwrap();
    let observed = Arc::new(Mutex::new(Observed::default()));
    let mut state = WorkbenchState::new(DemoSource(Vec::new()), &Default::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_provider_status(ProviderStatus::Configured)
        .with_ownership(host)
        .with_command_sink(Box::new(Sink(observed.clone())));
    state.composer_mut().input = "hello".into();
    (state, observed)
}

#[test]
fn submit_autostarts_unowned_thread() {
    // Given
    let dir = tempfile::tempdir().unwrap();
    let owner = host(&dir);
    let (mut state, observed) = state(owner.clone());
    // When
    state.submit_composer();
    // Then
    let observed = observed.lock().unwrap();
    assert_eq!(observed.commands.len(), 1);
    assert_eq!(observed.permits.len(), 1);
    assert_eq!(
        observed.permits[0].lease.owner_id,
        owner.owned_permit("thread").unwrap().lease.owner_id
    );
}

#[test]
fn submit_sends_when_self_owned() {
    // Given
    let dir = tempfile::tempdir().unwrap();
    let owner = host(&dir);
    let permit = owner.start("thread").unwrap();
    let (mut state, observed) = state(owner);
    // When
    state.submit_composer();
    // Then
    let observed = observed.lock().unwrap();
    assert_eq!(observed.commands.len(), 1);
    assert_eq!(
        observed.permits[0].lease.generation,
        permit.lease.generation
    );
}

#[test]
fn owner_responsive_shows_notice_without_sending() {
    // Given: two independent hosts, with live sockets on the same registry.
    let dir = tempfile::tempdir().unwrap();
    let other = host(&dir);
    other.start("thread").unwrap();
    let (mut state, observed) = state(host(&dir));
    // When
    state.submit_composer();
    // Then: no sink invocation (and therefore no CommandRejected), draft preserved.
    assert!(observed.lock().unwrap().commands.is_empty());
    assert!(state.issued().is_empty());
    assert!(state.transcript().entries().iter().any(|entry| matches!(entry,
        TranscriptEntry::Notice { text } if text == "このスレッドは別ウィンドウが write mode で保持中です")));
    assert_eq!(state.composer_mut().input, "hello");
}

#[test]
fn exists_race_rechecks_owned_permit() {
    // Given: another submit won start after the initial absent observation.
    let dir = tempfile::tempdir().unwrap();
    let owner = host(&dir);
    owner.start("thread").unwrap();
    let result = owner.start("thread");
    assert!(matches!(result, Err(RegistryError::Exists)));
    // When
    let permit = gui::runtime_sink::finish_chat_start(&owner, "thread", result).unwrap();
    // Then
    permit.validate_generation().unwrap();
    assert_eq!(
        permit.lease.owner_id,
        owner.owned_permit("thread").unwrap().lease.owner_id
    );
}

struct HeldModel;
#[async_trait::async_trait]
impl runtime::AgentModel for HeldModel {
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        std::future::pending().await
    }
    fn selected_model(&self, _: runtime::Role) -> String {
        "test".into()
    }
}

fn runtime_sink(
    host: Arc<OwnerHost>,
) -> (
    tokio::runtime::Runtime,
    gui::runtime_sink::RuntimeCommandSink,
    runtime::AgentRuntime,
) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let bus = Arc::new(EventBus::new(64));
    let runtime = runtime::AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus.clone())),
        Arc::new(HeldModel),
    );
    let supervisor = rt.block_on(async {
        runtime::GoalSupervisor::spawn(
            runtime.clone(),
            bus,
            Arc::new(runtime::FixtureDeliveryAdapter::default()),
            Default::default(),
        )
    });
    let sink = gui::runtime_sink::RuntimeCommandSink::new(
        runtime.clone(),
        rt.handle().clone(),
        supervisor,
    )
    .with_ownership(host);
    (rt, sink, runtime)
}

#[test]
fn composer_autostart_reaches_real_runtime() {
    // Given
    let dir = tempfile::tempdir().unwrap();
    let owner = host(&dir);
    let (_rt, sink, runtime) = runtime_sink(owner.clone());
    let (state, _) = state(owner);
    let mut state = state.with_command_sink(Box::new(sink));
    // When
    state.submit_composer();
    // Then
    assert_eq!(state.issued().len(), 1);
    assert_eq!(runtime.list_agents().len(), 1);
    assert!(state.composer_mut().input.is_empty());
}

#[test]
fn sink_rejects_old_permit_even_after_same_host_reclaims() {
    // Given: same host ID but a new generation before sink dispatch.
    let dir = tempfile::tempdir().unwrap();
    let owner = host(&dir);
    let stale = owner.start("thread").unwrap();
    owner.quiesce().unwrap();
    owner.claim(&owner.attach("thread").unwrap()).unwrap();
    let (_rt, mut sink, runtime) = runtime_sink(owner);
    // When
    let events = sink.submit_chat_with_permit(
        ChatSubmission {
            images: Vec::new(),
            thread_id: "thread".into(),
            text: "hello".into(),
            model_preference: None,
        },
        stale,
    );
    // Then
    assert!(matches!(
        events.as_slice(),
        [LoopEvent::ChatRejected { .. }]
    ));
    assert!(runtime.list_agents().is_empty());
}

#[test]
fn exists_race_with_other_owner_does_not_return_a_permit() {
    // Given
    let dir = tempfile::tempdir().unwrap();
    let other = host(&dir);
    other.start("thread").unwrap();
    let owner = host(&dir);
    // When
    let result = gui::runtime_sink::finish_chat_start(&owner, "thread", owner.start("thread"));
    // Then
    assert!(result.is_err());
}

#[test]
fn renewed_generation_does_not_reuse_previous_chat_run() {
    // Given
    let dir = tempfile::tempdir().unwrap();
    let owner = host(&dir);
    let first = owner.start("thread").unwrap();
    let (_rt, mut sink, _) = runtime_sink(owner.clone());
    let chat = ChatSubmission {
        images: Vec::new(),
        thread_id: "thread".into(),
        text: "hello".into(),
        model_preference: None,
    };
    let original = sink.submit_chat_with_permit(chat.clone(), first.clone());
    let mut registry = runtime::ownership::Registry::open(&first.registry_path).unwrap();
    registry
        .update("thread", |owner| {
            owner.lease.generation += 1;
            owner.active_turn = false;
            owner.active_runs.clear();
            Ok(())
        })
        .unwrap();
    // When
    let renewed = sink.submit_chat_with_permit(chat, owner.owned_permit("thread").unwrap());
    // Then
    let [LoopEvent::ChatAccepted { run_id: first, .. }] = original.as_slice() else {
        panic!("first accepted")
    };
    let [LoopEvent::ChatAccepted { run_id: second, .. }] = renewed.as_slice() else {
        panic!("second accepted")
    };
    assert_ne!(first, second);
}
