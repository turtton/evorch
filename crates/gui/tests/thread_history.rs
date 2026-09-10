use event_bus::{Event, LifecycleEvent, MessageEvent};
use gui::model::composer::ProviderStatus;
use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};
use storage::{Database, Storage, StorageConfig};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

fn state(root: &std::path::Path) -> WorkbenchState<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", root)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    for id in ["one", "two"] {
        sidebar
            .create_thread(ThreadId::new(id), project.clone(), id)
            .unwrap();
    }
    sidebar.switch_thread(&ThreadId::new("one")).unwrap();
    WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_sidebar_path(root.join("sidebar.json"))
        .with_provider_status(ProviderStatus::Configured)
}

fn reply(run: &str, thread: &str, text: &str) -> [Event; 2] {
    [
        Event::new(LifecycleEvent::AgentRunStarted {
            run_id: run.into(),
            parent_run_id: None,
            agent_name: format!("chat:{thread}"),
            role: "worker".into(),
        }),
        Event::new(MessageEvent::MessageDelta {
            run_id: Some(run.into()),
            delta: text.into(),
        }),
    ]
}

#[test]
fn switching_thread_updates_chat_pane() {
    // Given: two conversations, including a reply arriving in the background.
    let dir = tempfile::tempdir().unwrap();
    let mut state = state(dir.path());
    state.apply_events(reply("run-1", "one", "first reply"));
    state.switch_thread(ThreadId::new("two")).unwrap();
    state.apply_events(reply("run-2", "two", "second reply"));
    state.apply_events([Event::new(MessageEvent::MessageDelta {
        run_id: Some("run-1".into()),
        delta: " in background".into(),
    })]);
    let mut gui = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    gui.run();
    assert!(gui.has_label("second reply"));
    assert!(!gui.has_label("first reply in background"));
    // When: the sidebar selects the first conversation.
    gui.click_label("one");
    gui.run();
    // Then: only the selected conversation is rendered.
    assert!(gui.has_label("first reply in background"));
    assert!(!gui.has_label("second reply"));
    if let Some(directory) = std::env::var_os("THREAD_EVIDENCE_DIR") {
        gui.capture()
            .unwrap()
            .save_png(&std::path::PathBuf::from(directory).join("thread-one.png"))
            .unwrap();
    }
}

#[test]
fn thread_history_is_restored_on_reopen() {
    // Given: persisted events and a user message from a previous GUI instance.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let mut original = state(dir.path());
    original.composer_mut().input = "remember this".into();
    original.submit_composer();
    for event in reply("chat-1", "one", "remembered reply") {
        storage.handle().append_event(Some("gui"), &event).unwrap();
        original.apply_events([event]);
    }
    original.save_sidebar();
    drop(original);
    // When: a new workbench restores the sidebar and database.
    let sidebar = workspace_ui::load_sidebar(&dir.path().join("sidebar.json")).unwrap();
    let mut reopened = state(dir.path()).with_sidebar(sidebar);
    reopened
        .restore_history(&Database::open(&config).unwrap())
        .unwrap();
    let mut gui = HeadlessWorkbench::new(reopened, [1200.0, 900.0]);
    gui.run();
    // Then: both sides of the conversation are visible without a new run.
    assert!(gui.has_label("You: remember this"));
    assert!(gui.has_label("remembered reply"));
    if let Some(directory) = std::env::var_os("THREAD_EVIDENCE_DIR") {
        gui.capture()
            .unwrap()
            .save_png(&std::path::PathBuf::from(directory).join("thread-restored.png"))
            .unwrap();
    }
}

#[test]
fn thread_list_and_selection_state_persists() {
    // Given: a sidebar persistence path.
    let dir = tempfile::tempdir().unwrap();
    let mut original = state(dir.path());
    // When: creating and selecting a new conversation.
    let selected = original.create_thread("third").unwrap();
    drop(original);
    // Then: the list and selection survive reconstruction.
    let saved = workspace_ui::load_sidebar(&dir.path().join("sidebar.json")).unwrap();
    assert_eq!(saved.threads.len(), 3);
    assert_eq!(saved.active_thread, Some(selected));
}

#[test]
fn unknown_run_does_not_leak_into_selected_thread() {
    // Given: an active conversation with no binding for this run.
    let dir = tempfile::tempdir().unwrap();
    let mut state = state(dir.path());
    // When: an unrelated run emits a reply.
    state.apply_events([Event::new(MessageEvent::MessageDelta {
        run_id: Some("unrelated".into()),
        delta: "unrelated reply".into(),
    })]);
    // Then: the run retains it, but the selected conversation remains empty.
    assert!(state.transcript().entries().is_empty());
    assert_eq!(
        state
            .transcripts()
            .run("unrelated")
            .unwrap()
            .entries()
            .len(),
        1
    );
}
