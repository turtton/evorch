use event_bus::{AgentRunPhase, Event, LifecycleEvent, MessageEvent, ToolEvent};
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

fn prompt(run: &str, parent: Option<&str>, name: &str, text: &str) -> Event {
    Event::new(LifecycleEvent::TaskPromptPublished {
        run_id: run.into(),
        parent_run_id: parent.map(str::to_owned),
        agent_name: name.into(),
        role: "reviewer".into(),
        prompt: text.into(),
    })
}

fn final_result(run: &str, text: &str) -> Event {
    Event::new(MessageEvent::FinalResultPublished {
        run_id: run.into(),
        text: text.into(),
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
                prompt("root", None, name, "investigate this"),
                Event::new(ToolEvent::ToolStarted {
                    tool_name: "read".into(),
                    call_id: "call".into(),
                    run_id: Some("root".into()),
                    input: Some(serde_json::json!({"path":"src/main.rs"})),
                    parent_call_id: None,
                }),
                Event::new(ToolEvent::ToolCompleted {
                    tool_name: "read".into(),
                    call_id: "call".into(),
                    run_id: Some("root".into()),
                    output: Some("file contents".into()),
                    is_error: false,
                    detail: None,
                    parent_call_id: None,
                }),
                delta("root", "found the cause"),
                final_result("root", "found the cause"),
                Event::new(LifecycleEvent::AgentRunStateChanged {
                    run_id: "root".into(),
                    from: AgentRunPhase::Running,
                    to: AgentRunPhase::Done,
                    reason: None,
                }),
            ],
        );
        storage.close();
        let expected = original.transcript().entries().to_vec();
        assert_eq!(expected.iter().filter(|entry| matches!(entry, TranscriptEntry::UserMessage { text } if text == "investigate this")).count(), 1);
        assert_eq!(expected.iter().filter(|entry| matches!(entry, TranscriptEntry::Message { text, .. } if text == "found the cause")).count(), 1);
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
fn past_subagents_remain_browsable_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let mut original = state(dir.path(), &["one", "two"]);
    persist_and_apply(
        &storage,
        &mut original,
        vec![
            started("run-1", None, "chat:one"),
            started("run-2", Some("run-1"), "agent one"),
            delta("run-2", "result from first child"),
            Event::new(LifecycleEvent::AgentRunStateChanged {
                run_id: "run-2".into(),
                from: AgentRunPhase::Running,
                to: AgentRunPhase::Done,
                reason: None,
            }),
            started("run-3", None, "chat:two"),
            started("run-4", Some("run-3"), "agent two"),
            delta("run-4", "result from second child"),
            Event::new(LifecycleEvent::AgentRunStateChanged {
                run_id: "run-4".into(),
                from: AgentRunPhase::Running,
                to: AgentRunPhase::Done,
                reason: None,
            }),
        ],
    );
    original.switch_thread(ThreadId::new("two")).unwrap();
    storage.close();

    let sidebar = workspace_ui::load_sidebar(&dir.path().join("sidebar.json")).unwrap();
    let mut reopened = state(dir.path(), &["one", "two"]).with_sidebar(sidebar);
    let database = Database::open(&config).unwrap();
    reopened.restore_history(&database).unwrap();
    reopened.restore_history(&database).unwrap();

    let mut gui = gui::headless::HeadlessWorkbench::new(reopened, [1200.0, 900.0]);
    gui.run();
    assert!(gui.has_label("agent two"));
    assert!(!gui.has_label("agent one"));
    gui.click_label("agent two");
    gui.run();
    assert!(gui.has_label("result from second child"));

    gui.state_mut().switch_thread(ThreadId::new("one")).unwrap();
    gui.run();
    assert!(gui.has_label("agent one"));
    assert!(!gui.has_label("agent two"));
}

#[test]
fn published_prompt_and_final_result_are_ordered_and_idempotent_live_and_in_replay() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let mut live = state(dir.path(), &["one", "two"]);
    let instruction = prompt(
        "child",
        Some("root"),
        "reviewer",
        "Review the implementation",
    );
    persist_and_apply(
        &storage,
        &mut live,
        vec![
            started("root", None, "chat:Orchestrator:one"),
            started("child", Some("root"), "reviewer"),
            instruction.clone(),
            delta("child", "Checking the changes"),
            final_result("child", "Canonical review report"),
        ],
    );
    assert!(
        matches!(live.transcripts().run("child").unwrap().entries().last(),
        Some(TranscriptEntry::Message { text, .. }) if text == "Canonical review report"),
        "result is visible before the terminal event"
    );
    assert!(!live.transcript().entries().iter().any(|entry| matches!(
        entry, TranscriptEntry::UserMessage { text } if text == "Review the implementation"
    )));
    // A restore reuses the same run and must not duplicate its initial instruction.
    persist_and_apply(
        &storage,
        &mut live,
        vec![
            Event::new(LifecycleEvent::AgentRunStateChanged {
                run_id: "child".into(),
                from: AgentRunPhase::Running,
                to: AgentRunPhase::Done,
                reason: None,
            }),
            started("child", Some("root"), "reviewer"),
            instruction,
        ],
    );
    storage.close();
    let mut replay = state(dir.path(), &["one", "two"]).with_sidebar(live.sidebar().clone());
    let db = Database::open(&config).unwrap();
    for _ in 0..2 {
        replay.restore_history(&db).unwrap();
        assert_eq!(replay.transcript().entries(), live.transcript().entries());
        let expected = live.transcripts().run("child").unwrap().entries();
        assert_eq!(
            replay.transcripts().run("child").unwrap().entries(),
            expected
        );
        assert!(
            matches!(expected.first(), Some(TranscriptEntry::UserMessage { text }) if text == "Review the implementation")
        );
        assert_eq!(
            expected
                .iter()
                .filter(|entry| matches!(entry, TranscriptEntry::UserMessage { .. }))
                .count(),
            1
        );
        let result = expected
            .iter()
            .position(|entry| {
                matches!(entry,
            TranscriptEntry::Message { text, .. } if text == "Canonical review report")
            })
            .unwrap();
        let terminal = expected
            .iter()
            .position(|entry| {
                matches!(entry,
            TranscriptEntry::Notice { text } if text == "subagent reviewer completed")
            })
            .unwrap();
        assert!(result < terminal);
        assert!(
            !replay
                .transcript()
                .entries()
                .iter()
                .any(|entry| matches!(entry,
            TranscriptEntry::Message { text, .. } if text == "Canonical review report")),
            "child result stays in its own run"
        );
    }
}

#[test]
fn learning_reviewer_root_prompt_never_appears_in_main_thread_live_or_replay() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let mut live = state(dir.path(), &["one"]);
    persist_and_apply(
        &storage,
        &mut live,
        vec![
            started("learning-reviewer", None, "learning-reviewer"),
            prompt(
                "learning-reviewer",
                None,
                "learning-reviewer",
                "Review this completed run for learning",
            ),
            final_result("learning-reviewer", "Learning review complete"),
        ],
    );
    storage.close();
    let mut replay = state(dir.path(), &["one"]);
    replay
        .restore_history(&Database::open(&config).unwrap())
        .unwrap();
    for state in [&live, &replay] {
        assert!(state.transcript().entries().is_empty());
        assert_eq!(
            state
                .transcripts()
                .run("learning-reviewer")
                .unwrap()
                .entries(),
            &[
                TranscriptEntry::UserMessage {
                    text: "Review this completed run for learning".into()
                },
                TranscriptEntry::Message {
                    text: "Learning review complete".into(),
                    run_id: Some("learning-reviewer".into())
                },
            ]
        );
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

#[test]
fn presented_artifacts_replay_as_the_same_card_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let mut original = state(dir.path(), &["one"]);
    original.composer_mut().input = "show me a login mock".into();
    original.submit_composer();
    let presentation = event_bus::ArtifactPresentation {
        presentation_id: "presentation-1".into(),
        title: Some("Login".into()),
        caption: Some("Two layouts".into()),
        artifacts: vec![event_bus::PresentedArtifact {
            artifact_id: "artifact-1".into(),
            title: "Variant A".into(),
            caption: None,
            media_type: "text/html".into(),
            path: dir.path().join("a.html").to_string_lossy().into_owned(),
            byte_len: 10,
            sha256: "a".repeat(64),
        }],
    };
    let name = "chat:Worker:one";
    persist_and_apply(
        &storage,
        &mut original,
        vec![
            started("root", None, name),
            prompt("root", None, name, "show me a login mock"),
            Event::new(ToolEvent::ArtifactsPresented {
                run_id: "root".into(),
                presentation: presentation.clone(),
            }),
            final_result("root", "Here are the mocks"),
        ],
    );
    storage.close();
    let expected = original.transcript().entries().to_vec();
    assert!(expected.contains(&TranscriptEntry::Artifacts { presentation }));

    let sidebar = workspace_ui::load_sidebar(&dir.path().join("sidebar.json")).unwrap();
    let mut reopened = state(dir.path(), &["one"]).with_sidebar(sidebar);
    reopened
        .restore_history(&Database::open(&config).unwrap())
        .unwrap();
    assert_eq!(reopened.transcript().entries(), expected);
}

#[test]
fn history_with_removed_supervisor_events_still_restores() {
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
        vec![started("root", None, "chat:Orchestrator:one")],
    );
    rusqlite::Connection::open(&config.db_path)
        .unwrap()
        .execute(
            "INSERT INTO events \
             (session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload) \
             VALUES ('gui', ?1, 0, 0, 'Orchestrator', ?2)",
            rusqlite::params![
                event_bus::SCHEMA_VERSION,
                r#"{"kind":"Orchestrator","payload":{"kind":"MergeStateChanged","payload":{"goal_id":"goal"}}}"#
            ],
        )
        .unwrap();
    persist_and_apply(&storage, &mut original, vec![delta("root", "kept reply")]);
    storage.close();
    let sidebar = workspace_ui::load_sidebar(&dir.path().join("sidebar.json")).unwrap();
    let mut reopened = state(dir.path(), &["one"]).with_sidebar(sidebar);
    reopened
        .restore_history(&Database::open(&config).unwrap())
        .unwrap();
    assert_eq!(
        reopened.transcript().entries(),
        original.transcript().entries()
    );
    assert!(reopened.transcript().entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Message { text, .. } if text == "kept reply"
    )));
}

#[test]
fn access_reviews_replay_inside_their_tool_cards_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let mut original = state(dir.path(), &["one"]);
    original.composer_mut().input = "fetch the release notes".into();
    original.submit_composer();
    let name = "chat:Worker:one";
    let diagnostic = |source: &str, code: &str, detail: String, call: &str| {
        Event::new(event_bus::DiagnosticEvent {
            source: source.into(),
            severity: event_bus::DiagnosticSeverity::Warning,
            code: code.into(),
            detail,
            run_id: Some("root".into()),
            thread_id: None,
            call_id: Some(call.into()),
        })
    };
    // Given: a reviewed shell escape and a denied fetch are persisted.
    persist_and_apply(
        &storage,
        &mut original,
        vec![
            started("root", None, name),
            prompt("root", None, name, "fetch the release notes"),
            Event::new(ToolEvent::ToolStarted {
                tool_name: "shell".into(),
                call_id: "call-shell".into(),
                input: Some(serde_json::json!({"command": "curl example.com"})),
                run_id: Some("root".into()),
                parent_call_id: None,
            }),
            diagnostic(
                "sandbox",
                "escalation_review",
                "denied: network is not needed".into(),
                "call-shell",
            ),
            Event::new(ToolEvent::ToolCompleted {
                tool_name: "shell".into(),
                call_id: "call-shell".into(),
                is_error: true,
                output: Some("denied".into()),
                detail: None,
                run_id: Some("root".into()),
                parent_call_id: None,
            }),
            diagnostic(
                "network_access",
                "tool_call_access",
                serde_json::json!({"tool": "web_fetch", "input": {"url": "https://example.com"},
                    "decision": "deny", "reviewer": "tool_policy", "reason": "web tools are disabled"})
                .to_string(),
                "call-fetch",
            ),
        ],
    );
    storage.close();
    let expected = original.transcript().entries().to_vec();
    let tools: Vec<_> = expected
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Tool {
                tool_name,
                status,
                reviews,
                ..
            } => Some((tool_name.as_str(), status.clone(), reviews.len())),
            _ => None,
        })
        .collect();
    assert_eq!(
        tools,
        [
            ("shell", ToolStatus::Failed, 1),
            (
                "web_fetch",
                ToolStatus::Denied {
                    reason: "web tools are disabled".into()
                },
                1
            ),
        ]
    );
    assert!(!expected.iter().any(|entry| matches!(
        entry,
        TranscriptEntry::SandboxReview { .. } | TranscriptEntry::Notice { .. }
    )));
    // When: the conversation is restored from storage.
    let sidebar = workspace_ui::load_sidebar(&dir.path().join("sidebar.json")).unwrap();
    let mut reopened = state(dir.path(), &["one"]).with_sidebar(sidebar);
    reopened
        .restore_history(&Database::open(&config).unwrap())
        .unwrap();
    // Then: each review is attached to the same card as it was live.
    assert_eq!(reopened.transcript().entries(), expected);
}
