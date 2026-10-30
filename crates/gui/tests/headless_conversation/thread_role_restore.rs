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
        host_inputs_json: "[]".into(),
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
    assert!(state.composer().role_locked);
    state.composer_mut().toggle_role();
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
fn persisted_role_changed_by_old_ui_recovers_first_root_role() {
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
    sidebar.threads[0].chat_role = Some(ThreadChatRole::Worker);
    let path = dir.path().join("sidebar.json");
    let mut state = WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_sidebar_path(path.clone());
    state.save_sidebar();

    state.restore_history(&db).unwrap();

    assert_eq!(state.composer().role, ComposerRole::Orchestrator);
    assert!(state.composer().role_locked);
    state.composer_mut().toggle_role();
    assert_eq!(state.composer().role, ComposerRole::Orchestrator);
    assert_eq!(
        workspace_ui::load_sidebar(&path).unwrap().threads[0].chat_role,
        Some(ThreadChatRole::Orchestrator)
    );
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

#[test]
fn stopped_history_replays_as_resumable_with_role_and_partial_output_intact() {
    use event_bus::{AgentRunPhase, Event, LifecycleEvent, MessageEvent};
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("stop.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let db = Database::open(&config).unwrap();
    storage
        .handle()
        .upsert_run_context(&context(
            "run-200",
            "Orchestrator",
            "chat:Orchestrator:old",
            "Stopped",
        ))
        .unwrap();
    for event in [
        Event::new(LifecycleEvent::AgentRunStarted {
            run_id: "run-200".into(),
            parent_run_id: None,
            agent_name: "chat:Orchestrator:old".into(),
            role: "Orchestrator".into(),
        }),
        Event::new(MessageEvent::MessageDelta {
            run_id: Some("run-200".into()),
            delta: "kept output".into(),
        }),
        Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "run-200".into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Stopped,
            reason: None,
        }),
    ] {
        storage.handle().append_event(Some("gui"), &event).unwrap();
    }
    let mut sidebar = sidebar(dir.path());
    sidebar.threads[0].run_ids.push("run-200".into());
    let mut state = WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar);
    state.restore_history(&db).unwrap();
    assert_eq!(state.composer().role, ComposerRole::Orchestrator);
    assert_eq!(
        state.thread_phases()["run-200"],
        workspace_ui::ThreadRunPhase::Stopped
    );
    assert!(state.transcript().entries().iter().any(|entry| matches!(entry, gui::model::transcript::TranscriptEntry::Message { text, .. } if text == "kept output")));
    let mut harness = gui::headless::HeadlessWorkbench::new(state, [1200.0, 900.0]);
    harness.run();
    assert!(!harness.has_label("Stopped (resumable) — send a message or /continue to resume"));
    assert!(harness.has_label("Send"));
}

/// Replays a history with an incomplete second answer after the storage
/// retention tick ran with `retention_days`, returning the restored answers
/// of the run and the provider observations left in the database.
fn restored_after_retention(retention_days: u32) -> (Vec<String>, Vec<String>) {
    use event_bus::{AgentRunPhase, Event, EventKind, LifecycleEvent, MessageEvent};
    use event_bus::{ProviderEvent, RequestPurpose};
    use gui::model::transcript::TranscriptEntry;
    use providers::{ContentBlock, Message, Role};

    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        provider_event_retention_days: retention_days,
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let db = Database::open(&config).unwrap();
    let mut record = context("run-104", "Worker", "chat:Worker:old", "Checkpoint");
    record.messages_json = serde_json::to_string(&vec![Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text {
            text: "complete second answer".into(),
        }],
    }])
    .unwrap();
    storage.handle().upsert_run_context(&record).unwrap();
    let run_id = || Some("run-104".to_owned());
    let request = |id: &str| ProviderEvent::RequestStarted {
        request_id: id.into(),
        provider: "p".into(),
        profile: None,
        protocol: "x".into(),
        model: "m".into(),
        streaming: true,
        run_id: run_id(),
        reasoning_effort: None,
        purpose: None::<RequestPurpose>,
    };
    let first_token = |id: &str| ProviderEvent::FirstTokenObserved {
        request_id: id.into(),
        provider: "p".into(),
        profile: None,
        protocol: "x".into(),
        model: "m".into(),
        ttft_ms: 5,
        run_id: run_id(),
    };
    let completed = |id: &str| ProviderEvent::RequestCompleted {
        request_id: id.into(),
        provider: "p".into(),
        profile: None,
        protocol: "x".into(),
        model: "m".into(),
        streaming: true,
        duration_ms: 10,
        input_tokens: 1,
        output_tokens: 1,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        finish_reason: "stop".into(),
        service_tier: None,
        run_id: run_id(),
        purpose: None,
        reasoning_tokens: None,
    };
    let waiting = || {
        Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "run-104".into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Waiting,
            reason: None,
        })
    };
    // Request A answers and waits; request B loses its answer and waits.
    let events: Vec<Event> = vec![
        Event::new(LifecycleEvent::AgentRunStarted {
            run_id: "run-104".into(),
            parent_run_id: None,
            agent_name: "chat:Worker:old".into(),
            role: "worker".into(),
        }),
        Event::new(request("a")),
        Event::new(first_token("a")),
        Event::new(MessageEvent::MessageCompleted {
            run_id: "run-104".into(),
            text: "first answer".into(),
        }),
        Event::new(completed("a")),
        waiting(),
        Event::new(request("b")),
        Event::new(first_token("b")),
        Event::new(MessageEvent::MessageDelta {
            run_id: run_id(),
            delta: "incomplete".into(),
        }),
        Event::new(completed("b")),
        waiting(),
    ];
    for mut event in events {
        // Old enough to be outside any retention window.
        event.meta.wall_clock = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        storage.handle().append_event(Some("gui"), &event).unwrap();
    }
    storage.handle().checkpoint_now().unwrap();

    let mut sidebar = sidebar(dir.path());
    sidebar.threads[0].run_ids.push("run-104".into());
    let mut state = WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_sidebar_path(dir.path().join("sidebar.json"));
    state.save_sidebar();
    state.restore_history(&db).unwrap();

    let answers = state
        .transcript()
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Message { text, run_id } if run_id.as_deref() == Some("run-104") => {
                Some(text.clone())
            }
            _ => None,
        })
        .collect();
    let mut observations = Vec::new();
    db.for_each_event_ordered(|stored| {
        if let EventKind::Provider(event) = &stored.event.kind {
            observations.push(format!("{event:?}").split(' ').next().unwrap().to_owned());
        }
    })
    .unwrap();
    (answers, observations)
}

#[test]
fn provider_retention_does_not_change_the_restored_final_answer() {
    // Given/When: the same history restored without and with retention
    let (kept_answers, kept) = restored_after_retention(0);
    let (answers, remaining) = restored_after_retention(90);

    // Then: the snapshot still repairs the second answer, as it does without
    // retention, because the request boundary survives expiry.
    assert_eq!(kept_answers, vec!["first answer", "complete second answer"]);
    assert_eq!(answers, kept_answers);
    assert_eq!(kept.len(), 6);
    assert_eq!(remaining, vec!["RequestStarted", "RequestStarted"]);
}
