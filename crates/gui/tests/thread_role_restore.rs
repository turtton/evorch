use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::model::composer::ComposerRole;
use storage::{Database, RunContextRecord, Storage, StorageConfig};
use workspace_ui::{ProjectId, SidebarState, ThreadChatRole, ThreadId, UiSettings};

fn sidebar(root: &std::path::Path) -> SidebarState {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("evorch");
    sidebar
        .add_project(project.clone(), "evorch", root)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    for id in ["old", "other"] {
        sidebar
            .create_thread(ThreadId::new(id), project.clone(), id)
            .unwrap();
    }
    sidebar.switch_thread(&ThreadId::new("old")).unwrap();
    sidebar
}

fn context(run_id: &str, role: &str, name: &str, phase: &str) -> RunContextRecord {
    RunContextRecord {
        run_id: run_id.into(),
        role: role.into(),
        name: name.into(),
        parent_run_id: None,
        config_json: "{}".into(),
        messages_json: "[]".into(),
        checkpoints_json: "[]".into(),
        terminal_phase: phase.into(),
        restorable: false,
        updated_at_ns: 1,
    }
}

#[test]
fn legacy_thread_recovers_its_original_chat_role_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let db = Database::open(&config).unwrap();
    storage
        .handle()
        .upsert_run_context(&context(
            "run-95",
            "Orchestrator",
            "chat:Orchestrator:old",
            "Done",
        ))
        .unwrap();
    storage
        .handle()
        .upsert_run_context(&context("run-97", "Worker", "chat:Worker:old", "Error"))
        .unwrap();
    let mut sidebar = sidebar(dir.path());
    sidebar.threads[0].run_ids = vec!["run-95".into(), "run-97".into()];
    let path = dir.path().join("sidebar.json");
    let mut state = WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_sidebar_path(path.clone());
    state.save_sidebar();
    assert_eq!(state.composer().role, ComposerRole::Worker);

    state.restore_history(&db).unwrap();

    assert_eq!(state.composer().role, ComposerRole::Orchestrator);
    assert_eq!(
        state.sidebar().threads[0].chat_role,
        Some(ThreadChatRole::Orchestrator)
    );
    let persisted = workspace_ui::load_sidebar(&path).unwrap();
    assert_eq!(
        persisted.threads[0].chat_role,
        Some(ThreadChatRole::Orchestrator)
    );
    let mut state = state.with_provider_status(gui::model::composer::ProviderStatus::Configured);
    state.composer_mut().input = "continue implementation".into();
    state.submit_composer();
    assert!(matches!(
        state.issued().last(),
        Some(gui::model::commands::WorkbenchCommand::SendChat(submission))
            if submission.composer_role == ComposerRole::Orchestrator
    ));
}

#[test]
fn switching_threads_restores_each_chat_role() {
    let dir = tempfile::tempdir().unwrap();
    let mut sidebar = sidebar(dir.path());
    sidebar.threads[0].chat_role = Some(ThreadChatRole::Worker);
    sidebar.threads[1].chat_role = Some(ThreadChatRole::Orchestrator);
    let mut state = WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_sidebar_path(dir.path().join("sidebar.json"));

    assert_eq!(state.composer().role, ComposerRole::Worker);
    state.switch_thread(ThreadId::new("other")).unwrap();
    assert_eq!(state.composer().role, ComposerRole::Orchestrator);
    state.switch_thread(ThreadId::new("old")).unwrap();
    assert_eq!(state.composer().role, ComposerRole::Worker);
}

#[test]
fn legacy_checkpoint_repairs_missing_stream_chunks_in_display_history() {
    use event_bus::{AgentRunPhase, Event, LifecycleEvent, MessageEvent};
    use gui::model::transcript::TranscriptEntry;
    use providers::{ContentBlock, Message, Role};

    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let db = Database::open(&config).unwrap();
    let mut record = context("run-102", "Worker", "chat:Worker:old", "Checkpoint");
    record.messages_json = serde_json::to_string(&vec![Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text {
            text: "complete answer".into(),
        }],
    }])
    .unwrap();
    storage.handle().upsert_run_context(&record).unwrap();
    for event in [
        Event::new(LifecycleEvent::AgentRunStarted {
            run_id: "run-102".into(),
            parent_run_id: None,
            agent_name: "chat:Worker:old".into(),
            role: "worker".into(),
        }),
        Event::new(MessageEvent::MessageDelta {
            run_id: Some("run-102".into()),
            delta: "incomplete".into(),
        }),
        Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "run-102".into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Waiting,
            reason: None,
        }),
    ] {
        storage.handle().append_event(Some("gui"), &event).unwrap();
    }
    let mut sidebar = sidebar(dir.path());
    sidebar.threads[0].run_ids.push("run-102".into());
    let mut state = WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_sidebar_path(dir.path().join("sidebar.json"));
    state.save_sidebar();

    state.restore_history(&db).unwrap();

    let answers: Vec<_> = state
        .transcript()
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Message { text, run_id } if run_id.as_deref() == Some("run-102") => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(answers, vec!["complete answer"]);
}

#[test]
fn persisted_completion_is_not_replayed_twice_from_checkpoint() {
    use event_bus::{AgentRunPhase, Event, LifecycleEvent, MessageEvent};
    use gui::model::transcript::TranscriptEntry;
    use providers::{ContentBlock, Message, Role};

    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let db = Database::open(&config).unwrap();
    let mut record = context("run-103", "Worker", "chat:Worker:old", "Checkpoint");
    record.messages_json = serde_json::to_string(&vec![Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text {
            text: "complete answer".into(),
        }],
    }])
    .unwrap();
    storage.handle().upsert_run_context(&record).unwrap();
    for event in [
        Event::new(LifecycleEvent::AgentRunStarted {
            run_id: "run-103".into(),
            parent_run_id: None,
            agent_name: "chat:Worker:old".into(),
            role: "worker".into(),
        }),
        Event::new(MessageEvent::MessageDelta {
            run_id: Some("run-103".into()),
            delta: "incomplete".into(),
        }),
        Event::new(MessageEvent::MessageCompleted {
            run_id: "run-103".into(),
            text: "complete answer".into(),
        }),
        Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "run-103".into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Waiting,
            reason: None,
        }),
    ] {
        storage.handle().append_event(Some("gui"), &event).unwrap();
    }
    let mut sidebar = sidebar(dir.path());
    sidebar.threads[0].run_ids.push("run-103".into());
    let mut state = WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_sidebar_path(dir.path().join("sidebar.json"));
    state.save_sidebar();

    state.restore_history(&db).unwrap();

    let answers: Vec<_> = state
        .transcript()
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Message { text, run_id } if run_id.as_deref() == Some("run-103") => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(answers, vec!["complete answer"]);
}
