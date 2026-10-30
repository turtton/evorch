use event_bus::{Event, LifecycleEvent, MessageEvent};
use gui::model::composer::ProviderStatus;
use gui::model::transcript::TranscriptEntry;
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

#[test]
fn oversized_accepted_responses_restore_in_full_before_later_tools_and_responses() {
    use recorder::storage_bridge::{OwnedStorageBridge, StorageBridge};
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("parts.db"),
        hard_limits: storage::HardLimits {
            max_event_bytes: 512,
            ..Default::default()
        },
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let bus = std::sync::Arc::new(event_bus::EventBus::new(1024));
    let mut owner = OwnedStorageBridge::spawn(
        bus.clone(),
        storage,
        |handle| StorageBridge::new(handle, "session"),
        std::time::Duration::from_secs(3600),
    )
    .unwrap();
    let mut live = state(dir.path());
    let first_text = "あ\"\\\n🦀".repeat(300);
    let second_text = "次の応答\u{1}".repeat(300);
    let mut events = vec![reply("run", "one", "first partial")[0].clone()];
    for text in [&first_text, &second_text] {
        for chunk in ["first partial", " second partial", " third partial"] {
            events.push(Event::new(MessageEvent::MessageDelta {
                run_id: Some("run".into()),
                delta: chunk.into(),
            }));
        }
        events.push(Event::new(MessageEvent::MessageCompleted {
            run_id: "run".into(),
            text: text.clone(),
        }));
        events.push(Event::new(event_bus::ToolEvent::ToolStarted {
            tool_name: "read".into(),
            call_id: format!("run:{}", events.len()),
            input: None,
            run_id: Some("run".into()),
            parent_call_id: None,
        }));
    }
    events.push(Event::new(MessageEvent::MessageCompleted {
        run_id: "run".into(),
        text: "last response".into(),
    }));
    for event in &events {
        live.apply_events([event.clone()]);
        bus.emit(event.clone());
    }
    owner.flush().unwrap();
    owner.shutdown();
    drop(owner);
    let db = Database::open(&config).unwrap();
    let rows = db.events_all_ordered().unwrap();
    assert!(
        rows.iter()
            .all(|row| serde_json::to_vec(&row.event.kind).unwrap().len() <= 512)
    );
    assert!(!rows.iter().any(|row| matches!(
        row.event.kind,
        event_bus::EventKind::Message(MessageEvent::MessageDelta { .. })
    )));
    assert!(
        rows.iter()
            .filter(|row| matches!(
                row.event.kind,
                event_bus::EventKind::Message(MessageEvent::MessagePart { .. })
            ))
            .count()
            > 2
    );

    let mut reopened = state(dir.path());
    reopened.restore_history(&db).unwrap();
    assert_eq!(reopened.transcript().entries(), live.transcript().entries());
    let answers: Vec<_> = reopened
        .transcript()
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Message { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        answers,
        [first_text.as_str(), second_text.as_str(), "last response"]
    );
}

#[test]
fn stopped_and_failed_streams_do_not_restore_unaccepted_answer_drafts() {
    use event_bus::{AgentRunPhase, FaultEvent, SkillDiagnosticKind, ToolEvent};
    use recorder::storage_bridge::{OwnedStorageBridge, StorageBridge};
    for terminal in [AgentRunPhase::Stopped, AgentRunPhase::Error] {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("interrupted.db"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        let bus = std::sync::Arc::new(event_bus::EventBus::new(64));
        let mut owner = OwnedStorageBridge::spawn(
            bus.clone(),
            storage,
            |handle| StorageBridge::new(handle, "session"),
            std::time::Duration::from_secs(3600),
        )
        .unwrap();
        let thought = |text: &str| {
            Event::new(MessageEvent::ReasoningDelta {
                run_id: Some("run".into()),
                delta: text.into(),
            })
        };
        let events = [
            reply("run", "one", "")[0].clone(),
            thought("before approval"),
            Event::new(ToolEvent::ApprovalRequested {
                tool_name: "shell".into(),
                call_id: "unknown".into(),
                input: None,
            }),
            thought("before fault"),
            Event::new(FaultEvent::SkillDiagnostic {
                kind: SkillDiagnosticKind::DiscoveryError,
                skill: "example".into(),
                scope: "project".into(),
                detail: "unavailable".into(),
            }),
            thought("before stop"),
            Event::new(MessageEvent::MessageDelta {
                run_id: Some("run".into()),
                delta: "unaccepted draft".into(),
            }),
            Event::new(LifecycleEvent::AgentRunStateChanged {
                run_id: "run".into(),
                from: AgentRunPhase::Running,
                to: terminal,
                reason: Some("interrupted".into()),
            }),
        ];
        // Check the ordered transcript projection, independently of sidebar
        // ownership: run-less events cannot identify a particular thread.
        let mut durable_live = gui::model::transcript::TranscriptModel::new();
        for event in &events {
            // The persisted view intentionally excludes unaccepted answer text.
            if !matches!(
                event.kind,
                event_bus::EventKind::Message(MessageEvent::MessageDelta { .. })
            ) {
                durable_live.apply(event);
            }
            bus.emit(event.clone());
        }
        owner.flush().unwrap();
        owner.shutdown();
        drop(owner);
        let db = Database::open(&config).unwrap();
        let sessions = db.restore_sessions().unwrap();
        assert!(!sessions.is_empty());
        assert!(
            sessions
                .iter()
                .all(|session| session.pending_message.is_empty())
        );
        let mut reopened = gui::model::transcript::TranscriptModel::new();
        for row in db.events_all_ordered().unwrap() {
            reopened.apply(&row.event);
        }
        assert_eq!(reopened.entries(), durable_live.entries());
        assert!(!reopened.entries().iter().any(|entry| matches!(entry,
            TranscriptEntry::Message { text, .. } if text.contains("unaccepted"))));
        let reasoning: Vec<_> = reopened
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::Reasoning { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasoning,
            ["before approval", "before fault", "before stop"]
        );
    }
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
fn restored_thread_includes_assistant_events_from_storage() {
    // Given: the same sidebar and event writes used by the workbench.
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
        storage
            .handle()
            .append_event(Some("evorch-gui"), &event)
            .unwrap();
        original.apply_events([event]);
    }
    original.save_sidebar();
    drop(original);
    // When: a fresh workbench replays persisted history.
    let sidebar = workspace_ui::load_sidebar(&dir.path().join("sidebar.json")).unwrap();
    let mut reopened = state(dir.path()).with_sidebar(sidebar);
    reopened
        .restore_history(&Database::open(&config).unwrap())
        .unwrap();
    // Then: the transcript retains both participants, not just the sidebar's user text.
    let entries = reopened.transcript().entries();
    assert!(entries.iter().any(|entry| matches!(entry, TranscriptEntry::UserMessage { text, .. } if text == "remember this")), "restored user message missing: {entries:?}");
    assert!(entries.iter().any(|entry| matches!(entry, TranscriptEntry::Message { text, .. } if text == "remembered reply")), "restored assistant message missing: {entries:?}");
}

#[test]
fn events_appended_before_gui_close_survive_restart() {
    // Given: a previous GUI session has exhausted its event budget.
    let dir = tempfile::tempdir().unwrap();
    let previous = reply("old-run", "two", "old reply");
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        hard_limits: storage::HardLimits {
            max_session_bytes: previous
                .iter()
                .map(|event| u64::try_from(serde_json::to_vec(&event.kind).unwrap().len()).unwrap())
                .sum(),
            ..Default::default()
        },
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    for event in previous {
        storage
            .handle()
            .append_event(Some("evorch-gui"), &event)
            .unwrap();
    }
    storage.close();
    let storage = Storage::open(config.clone()).unwrap();
    let mut bridge = recorder::storage_bridge::StorageBridge::new(storage.handle(), "evorch-gui");
    let mut original = state(dir.path());
    original.composer_mut().input = "new question".into();
    original.submit_composer();
    // When: the real bridge acknowledges events, then the GUI and writer close.
    // Streamed answer text is live-only; the completed message is what persists.
    let completion = Event::new(MessageEvent::MessageCompleted {
        run_id: "new-run".into(),
        text: "new answer".into(),
    });
    for event in reply("new-run", "one", "new answer")
        .into_iter()
        .chain([completion])
    {
        let result = bridge.handle_event(&event);
        assert!(
            result.is_ok(),
            "new GUI events must persist after the previous session fills: {result:?}"
        );
        original.apply_events([event]);
    }
    original.save_sidebar();
    drop(original);
    drop(bridge);
    storage.close();
    let sidebar = workspace_ui::load_sidebar(&dir.path().join("sidebar.json")).unwrap();
    let mut reopened = state(dir.path()).with_sidebar(sidebar);
    reopened
        .restore_history(&Database::open(&config).unwrap())
        .unwrap();
    // Then: both sides survive a complete writer close and database reopen.
    let entries = reopened.transcript().entries();
    assert!(
        entries.iter().any(
            |entry| matches!(entry, TranscriptEntry::UserMessage { text, .. } if text == "new question")
        ),
        "restarted user message missing: {entries:?}"
    );
    assert!(
        entries.iter().any(
            |entry| matches!(entry, TranscriptEntry::Message { text, .. } if text == "new answer")
        ),
        "restarted assistant message missing: {entries:?}"
    );
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

#[test]
fn interrupted_reasoning_is_collapsed_when_history_is_restored() {
    // Given: persisted reasoning with no terminal event (an interrupted process).
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let started = reply("chat-1", "one", "unused")[0].clone();
    let thought = Event::new(MessageEvent::ReasoningDelta {
        run_id: Some("chat-1".into()),
        delta: "Interrupted thought".into(),
    });
    for event in [started, thought] {
        storage.handle().append_event(Some("gui"), &event).unwrap();
    }
    // When: the database is replayed into a new workbench.
    let mut reopened = state(dir.path());
    reopened
        .restore_history(&Database::open(&config).unwrap())
        .unwrap();
    let mut gui = HeadlessWorkbench::new(reopened, [1200.0, 900.0]);
    gui.run();
    // Then: replayed reasoning is historical, not an active stream.
    assert!(gui.has_label("thinking"));
    assert!(!gui.has_label("Interrupted thought"));
}
