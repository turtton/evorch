use event_bus::{AgentRunPhase, Event, LifecycleEvent, MessageEvent, OrchestratorEvent, ToolEvent};
use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::model::composer::ProviderStatus;
use gui::model::transcript::{ToolStatus, TranscriptEntry};
use storage::{Database, Storage, StorageConfig};
use workspace_ui::{PanelId, ProjectId, SidebarState, ThreadId, UiSettings};

fn state(root: &std::path::Path, threads: &[&str]) -> WorkbenchState<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", root)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    for id in threads {
        sidebar
            .create_thread(ThreadId::new(*id), project.clone(), *id)
            .unwrap();
    }
    sidebar.switch_thread(&ThreadId::new(threads[0])).unwrap();
    WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_sidebar_path(root.join("sidebar.json"))
        .with_provider_status(ProviderStatus::Configured)
}

fn started(run: &str, parent: Option<&str>, name: &str) -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: run.into(),
        parent_run_id: parent.map(str::to_owned),
        agent_name: name.into(),
        role: "orchestrator".into(),
    })
}

fn delta(run: &str, text: &str) -> Event {
    Event::new(MessageEvent::MessageDelta {
        run_id: Some(run.into()),
        delta: text.into(),
    })
}

fn persist_and_apply(
    storage: &Storage,
    state: &mut WorkbenchState<DemoSource>,
    events: Vec<Event>,
) {
    for event in events {
        storage.handle().append_event(Some("gui"), &event).unwrap();
        state.apply_events([event]);
    }
    state.save_sidebar();
}

#[test]
fn role_qualified_chat_restores_user_assistant_and_tool_entries() {
    for name in ["chat:one", "chat:Orchestrator:one", "chat:Worker:one"] {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("events.db"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        let mut original = state(dir.path(), &["one", "two"]);
        original.composer_mut().input = "investigate this".into();
        original.submit_composer();
        persist_and_apply(
            &storage,
            &mut original,
            vec![
                started("root", None, name),
                Event::new(ToolEvent::ToolStarted {
                    tool_name: "read".into(),
                    call_id: "call".into(),
                    run_id: Some("root".into()),
                    input: Some(serde_json::json!({"path":"src/main.rs"})),
                }),
                Event::new(ToolEvent::ToolCompleted {
                    tool_name: "read".into(),
                    call_id: "call".into(),
                    run_id: Some("root".into()),
                    output: Some("file contents".into()),
                    is_error: false,
                    detail: None,
                }),
                delta("root", "found the cause"),
            ],
        );
        storage.close();
        let expected = original.transcript().entries().to_vec();
        assert!(expected.iter().any(|entry| matches!(entry, TranscriptEntry::UserMessage { text } if text == "investigate this")));
        assert!(expected.iter().any(|entry| matches!(entry, TranscriptEntry::Message { text, .. } if text == "found the cause")));
        assert!(expected.iter().any(|entry| matches!(entry, TranscriptEntry::Tool { status: ToolStatus::Succeeded, output: Some(text), .. } if text == "file contents")));
        assert!(
            original
                .dock()
                .find_tab(&PanelId::new("agent-root"))
                .is_none()
        );

        // Rebuild ownership from events even if the last sidebar write was lost.
        let path = dir.path().join("sidebar.json");
        let saved = std::fs::read(&path).unwrap();
        for has_run_index in [true, false] {
            let mut sidebar = workspace_ui::load_sidebar(&path).unwrap();
            if !has_run_index {
                for thread in &mut sidebar.threads {
                    thread.run_ids.clear();
                }
            }
            let mut reopened = state(dir.path(), &["one", "two"]).with_sidebar(sidebar);
            let db = Database::open(&config).unwrap();
            for _ in 0..2 {
                reopened.restore_history(&db).unwrap();
                assert_eq!(
                    reopened.transcript().entries(),
                    expected,
                    "{name}, saved index={has_run_index}"
                );
                assert_eq!(
                    std::fs::read(&path).unwrap(),
                    saved,
                    "replay must not rewrite the sidebar"
                );
            }
        }
    }
}

#[test]
fn goal_children_and_continuation_replay_in_their_owning_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let mut original = state(dir.path(), &["one", "two"]);
    original.switch_thread(ThreadId::new("two")).unwrap();
    persist_and_apply(
        &storage,
        &mut original,
        vec![
            Event::new(OrchestratorEvent::GoalCreated {
                goal_id: "goal".into(),
                session_id: "gui".into(),
                project_id: "project".into(),
                thread_id: "one".into(),
                root_run_id: "root".into(),
                goal: "fix history".into(),
                references: Vec::new(),
                constraints: Vec::new(),
                repo: "owner/repo".into(),
                base_ref: "main".into(),
            }),
            started("root", None, "goal/orchestrator"),
            delta("root", "initial reply"),
            started("child", Some("root"), "investigator"),
            delta("child", "child details"),
            Event::new(LifecycleEvent::AgentRunStateChanged {
                run_id: "child".into(),
                from: AgentRunPhase::Running,
                to: AgentRunPhase::Done,
                reason: None,
            }),
            Event::new(LifecycleEvent::AgentRunStateChanged {
                run_id: "root".into(),
                from: AgentRunPhase::Running,
                to: AgentRunPhase::Error,
                reason: Some("provider failure".into()),
            }),
            Event::new(OrchestratorEvent::ContinuationDispatched {
                goal_id: "goal".into(),
                epoch: 1,
                trigger_run_id: "root".into(),
                new_run_id: "continuation".into(),
                unmet: Vec::new(),
            }),
            started("continuation", Some("root"), "goal/c1"),
            delta("continuation", "continued reply"),
            started("follow-up", Some("continuation"), "chat:Orchestrator:one"),
            delta("follow-up", "follow-up reply"),
            delta("root", "late root reply"),
            delta("continuation", "late continuation reply"),
        ],
    );
    storage.close();
    assert!(original.transcript().entries().is_empty());
    original.switch_thread(ThreadId::new("one")).unwrap();
    let expected = original.transcript().entries().to_vec();
    let messages: Vec<_> = expected
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Message { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        messages,
        ["initial reply", "continued reply", "follow-up reply"]
    );
    assert!(expected.iter().any(|entry| matches!(entry, TranscriptEntry::Notice { text } if text == "subagent investigator (child) completed")));
    assert!(expected.iter().any(|entry| matches!(entry, TranscriptEntry::Error { text } if text == "Run failed: provider failure")));
    assert!(
        original
            .dock()
            .find_tab(&PanelId::new("agent-continuation"))
            .is_none()
    );

    let path = dir.path().join("sidebar.json");
    let saved_sidebar = workspace_ui::load_sidebar(&path).unwrap();
    for has_run_index in [true, false] {
        let mut sidebar = saved_sidebar.clone();
        if !has_run_index {
            for thread in &mut sidebar.threads {
                thread.run_ids.clear();
            }
        }
        let mut reopened = state(dir.path(), &["one", "two"]).with_sidebar(sidebar);
        reopened
            .restore_history(&Database::open(&config).unwrap())
            .unwrap();
        assert_eq!(
            reopened.transcript().entries(),
            expected,
            "saved index={has_run_index}"
        );
        for run in ["root", "child", "continuation", "follow-up"] {
            assert_eq!(
                reopened.transcripts().run(run).unwrap().entries(),
                original.transcripts().run(run).unwrap().entries(),
                "run {run}"
            );
            assert!(
                reopened
                    .dock()
                    .find_tab(&PanelId::new(format!("agent-{run}")))
                    .is_none(),
                "replay must not reopen panes"
            );
        }
        reopened.switch_thread(ThreadId::new("two")).unwrap();
        assert!(reopened.transcript().entries().is_empty());
    }
}

#[test]
fn unrelated_root_never_attaches_to_the_only_active_thread() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let mut original = state(dir.path(), &["one"]);
    persist_and_apply(
        &storage,
        &mut original,
        vec![
            started("unrelated", None, "background-worker"),
            delta("unrelated", "unrelated reply"),
            Event::new(LifecycleEvent::AgentRunStateChanged {
                run_id: "unrelated".into(),
                from: AgentRunPhase::Running,
                to: AgentRunPhase::Done,
                reason: None,
            }),
        ],
    );
    storage.close();
    assert!(original.transcript().entries().is_empty());
    let sidebar = workspace_ui::load_sidebar(&dir.path().join("sidebar.json")).unwrap();
    assert!(sidebar.threads[0].run_ids.is_empty());
    let mut reopened = state(dir.path(), &["one"]).with_sidebar(sidebar);
    reopened
        .restore_history(&Database::open(&config).unwrap())
        .unwrap();
    assert!(reopened.transcript().entries().is_empty());
    assert_eq!(
        reopened.transcripts().run("unrelated").unwrap().entries(),
        original.transcripts().run("unrelated").unwrap().entries()
    );
}
