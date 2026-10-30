//! Differential replay against the pre-G1-B1 implementation, not a second
//! set of implementation-shaped assertions. Compare logical entries (not UI
//! instance IDs or wall-clock telemetry) before and after a feed continuation.
use super::{WorkbenchState, history::UserMessage};
use crate::{fixture::DemoSource, model::transcript::TranscriptEntry};
use event_bus::{AgentRunPhase, Event, LifecycleEvent, MessageEvent, ProviderEvent, ToolEvent};
use storage::{Database, RunContextRecord, Storage, StorageConfig};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

fn state(sidebar: SidebarState) -> WorkbenchState<DemoSource> {
    WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
}

fn phase(run: &str, to: AgentRunPhase) -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: run.into(),
        from: AgentRunPhase::Running,
        to,
        reason: None,
    })
}

fn started(run: &str, parent: Option<&str>, name: &str) -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: run.into(),
        parent_run_id: parent.map(str::to_owned),
        agent_name: name.into(),
        role: "Worker".into(),
    })
}

fn request(run: &str) -> Event {
    Event::new(ProviderEvent::RequestStarted {
        request_id: format!("request-{run}"),
        provider: "p".into(),
        profile: None,
        protocol: "p".into(),
        model: "m".into(),
        streaming: true,
        run_id: Some(run.into()),
        reasoning_effort: None,
        purpose: None,
    })
}

fn context(run: &str, thread: &str, messages: serde_json::Value) -> RunContextRecord {
    RunContextRecord {
        run_id: run.into(),
        role: "Worker".into(),
        name: format!("chat:Worker:{thread}"),
        parent_run_id: None,
        config_json: "{}".into(),
        messages_json: messages.to_string(),
        checkpoints_json: "[]".into(),
        terminal_phase: "Checkpoint".into(),
        restorable: false,
        updated_at_ns: 1,
        host_inputs_json: "[]".into(),
    }
}

fn assert_same(old: &WorkbenchState<DemoSource>, new: &WorkbenchState<DemoSource>) {
    assert_eq!(
        serde_json::to_value(&old.sidebar).unwrap(),
        serde_json::to_value(&new.sidebar).unwrap()
    );
    for thread in &old.sidebar.threads {
        let id = thread.id.to_string();
        let entries = |state: &WorkbenchState<DemoSource>| {
            state
                .transcripts
                .thread_model(&id)
                .map(|model| format!("{:?}", model.entries()))
        };
        assert_eq!(entries(old), entries(new), "thread {id}");
    }
    let runs: Vec<_> = old.transcripts.run_ids().collect();
    assert_eq!(runs, new.transcripts.run_ids().collect::<Vec<_>>());
    for run in runs {
        let entries = |state: &WorkbenchState<DemoSource>| {
            format!("{:?}", state.transcripts.run(run).unwrap().entries())
        };
        assert_eq!(entries(old), entries(new), "run {run}");
    }
    assert_eq!(old.phases, new.phases);
    assert_eq!(
        format!("{:?}", old.pending_approvals),
        format!("{:?}", new.pending_approvals)
    );
    assert_eq!(
        format!("{:?}", old.notifications),
        format!("{:?}", new.notifications)
    );
    old.system_notifications
        .assert_same_seed(&new.system_notifications);
    assert_eq!(old.shown_through_event, new.shown_through_event);
    assert_eq!(old.shared_feed.foreign_runs, new.shared_feed.foreign_runs);
    assert_eq!(
        format!("{:?}", old.shared_feed.parts),
        format!("{:?}", new.shared_feed.parts)
    );
    assert_eq!(
        serde_json::to_value(&old.history).unwrap(),
        serde_json::to_value(&new.history).unwrap()
    );
    assert_eq!(old.thread_goals, new.thread_goals);
    assert_eq!(old.thread_todos, new.thread_todos);
    assert_eq!(format!("{:?}", old.ledger), format!("{:?}", new.ledger));
}

#[test]
fn optimized_restore_matches_legacy_with_repairs_branches_escalation_and_feed_parts() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let db = Database::open(&config).unwrap();
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", dir.path())
        .unwrap();
    sidebar.select_project(&project).unwrap();
    for (thread, run) in [
        ("repair", "run-1"),
        ("complete", "run-2"),
        ("parts", "run-3"),
        ("skip", "run-4"),
        ("compressed-repair", "run-6"),
        ("compressed-complete", "run-7"),
    ] {
        sidebar
            .create_thread(ThreadId::new(thread), project.clone(), thread)
            .unwrap();
        sidebar.threads.last_mut().unwrap().run_ids.push(run.into());
    }
    sidebar
        .create_thread(ThreadId::new("fork"), project, "fork")
        .unwrap();
    let fork = sidebar.threads.last_mut().unwrap();
    fork.parent_thread_id = Some(ThreadId::new("repair"));
    fork.lineage = Some(workspace_ui::ThreadLineage {
        kind: workspace_ui::LineageKind::Fork,
        point: Some(workspace_ui::ForkPoint {
            run_id: "run-1".into(),
            context_len: 4,
        }),
    });
    sidebar.switch_thread(&ThreadId::new("repair")).unwrap();
    for (run, thread, messages) in [
        (
            "run-1",
            "repair",
            serde_json::json!([{"role":"assistant","content":[{"type":"text","text":"repaired answer"}]}]),
        ),
        (
            "run-2",
            "complete",
            serde_json::json!([{"role":"assistant","content":[{"type":"text","text":"must not duplicate"}]}]),
        ),
        ("run-3", "parts", serde_json::json!([])),
        (
            "run-4",
            "skip",
            serde_json::json!([{"role":"assistant","content":[{"type":"tool_use","id":"call","name":"read","input":{}}]}]),
        ),
    ] {
        storage
            .handle()
            .upsert_run_context(&context(run, thread, messages))
            .unwrap();
    }
    for (run, thread) in [
        ("run-6", "compressed-repair"),
        ("run-7", "compressed-complete"),
    ] {
        let messages = serde_json::json!([
            {"role":"user","content":[{"type":"text","text":"large history ".repeat(4096)}]},
            {"role":"assistant","content":[{"type":"text","text":"compressed answer"}]}
        ]);
        storage
            .handle()
            .upsert_run_context(&context(run, thread, messages))
            .unwrap();
    }
    let connection = rusqlite::Connection::open_with_flags(
        &config.db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    for (run, expected) in [
        ("run-1", "text"),
        ("run-2", "text"),
        ("run-6", "blob"),
        ("run-7", "blob"),
    ] {
        let kind: String = connection
            .query_row(
                "SELECT typeof(messages_json) FROM run_contexts WHERE run_id=?1",
                [run],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kind, expected);
    }
    let mut events = vec![
        started("run-1", None, "chat:Worker:repair"),
        request("run-1"),
        Event::new(MessageEvent::MessageCompleted {
            run_id: "run-1".into(),
            text: "earlier turn".into(),
        }),
        phase("run-1", AgentRunPhase::Waiting),
        Event::new(LifecycleEvent::TurnCompleted {
            run_id: "run-1".into(),
            context_len: 2,
        }),
        request("run-1"),
        Event::new(MessageEvent::MessageDelta {
            run_id: Some("run-1".into()),
            delta: "partial".into(),
        }),
        phase("run-1", AgentRunPhase::Waiting),
        Event::new(LifecycleEvent::TurnCompleted {
            run_id: "run-1".into(),
            context_len: 4,
        }),
        started("run-2", None, "chat:Worker:complete"),
        request("run-2"),
        Event::new(MessageEvent::MessageCompleted {
            run_id: "run-2".into(),
            text: "event answer".into(),
        }),
        phase("run-2", AgentRunPhase::Waiting),
        started("child", Some("run-1"), "child worker"),
        Event::new(MessageEvent::ReasoningDelta {
            run_id: Some("child".into()),
            delta: "private reasoning".into(),
        }),
        Event::new(ToolEvent::ToolStarted {
            run_id: Some("child".into()),
            tool_name: "shell".into(),
            call_id: "call".into(),
            input: Some(serde_json::json!({"command":"pwd"})),
            parent_call_id: None,
        }),
        Event::new(ToolEvent::ToolCompleted {
            run_id: Some("child".into()),
            tool_name: "shell".into(),
            call_id: "call".into(),
            output: Some("output".into()),
            detail: Some(serde_json::json!({"job_id":"job-1"})),
            is_error: false,
            parent_call_id: None,
        }),
        Event::new(ToolEvent::ApprovalRequested {
            tool_name: "write".into(),
            call_id: "pending".into(),
            input: Some(serde_json::json!({"path":"x"})),
        }),
        started("run-4", None, "chat:Worker:skip"),
        phase("run-4", AgentRunPhase::Waiting),
        Event::new(LifecycleEvent::EscalationRequested {
            source_run_id: "run-1".into(),
            new_run_id: "run-5".into(),
            summary: event_bus::EscalationMemoSummary {
                original_request: "request".into(),
                escalation_reason: "coordinate".into(),
                files_touched: vec![],
                blockers: vec![],
                suggested_next: "review".into(),
            },
        }),
        started("run-5", None, "escalation-orchestrator"),
        Event::new(MessageEvent::MessageCompleted {
            run_id: "run-5".into(),
            text: "escalated answer".into(),
        }),
        phase("run-5", AgentRunPhase::Done),
        started("run-6", None, "chat:Worker:compressed-repair"),
        request("run-6"),
        phase("run-6", AgentRunPhase::Waiting),
        started("run-7", None, "chat:Worker:compressed-complete"),
        request("run-7"),
        Event::new(MessageEvent::MessageCompleted {
            run_id: "run-7".into(),
            text: "compressed event answer".into(),
        }),
        phase("run-7", AgentRunPhase::Waiting),
        started("run-3", None, "chat:Worker:parts"),
        Event::new(MessageEvent::MessagePart {
            run_id: "run-3".into(),
            response_id: "response".into(),
            text: "first ".into(),
            index: 0,
            total: 2,
        }),
    ];
    // Timestamps are deliberately equal: row ID order is the replay contract.
    for event in &mut events {
        event.meta.wall_clock = std::time::SystemTime::UNIX_EPOCH;
    }
    for event in &events {
        storage.handle().append_event(Some("gui"), event).unwrap();
    }
    let registry_path = dir.path().join("owners.db");
    drop(runtime::ownership::Registry::open(&registry_path).unwrap());
    for registry_path in [None, Some(registry_path.as_path())] {
        let mut old = state(sidebar.clone());
        let mut new = state(sidebar.clone());
        let message: UserMessage = serde_json::from_value(serde_json::json!({
        "thread_id":"repair", "text":"user request", "at":{"secs_since_epoch":0,"nanos_since_epoch":0}
    })).unwrap();
        old.history.push(message.clone());
        new.history.push(message);
        old.restore_history_legacy(&db, registry_path).unwrap();
        match registry_path {
            Some(path) => new.restore_history_with_ownership(&db, path).unwrap(),
            None => new.restore_history(&db).unwrap(),
        }
        assert_same(&old, &new);
        assert_eq!(new.shown_through_event, db.last_event_id().unwrap());
        assert!(new.transcripts.run("run-1").unwrap().entries().iter().any(
        |entry| matches!(entry, TranscriptEntry::Message { text, .. } if text == "repaired answer")
    ));
        assert!(
            new.transcripts
                .thread_model("fork")
                .unwrap()
                .entries()
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::Branch { .. }))
        );
        assert!(
            new.sidebar
                .threads
                .iter()
                .any(|thread| thread.escalation_source_run_id.as_deref() == Some("run-1"))
        );
        // Replay must not reissue historical approval requests.
        assert_eq!(new.pending_approvals.items().count(), 0);
        let continuation = Event::new(MessageEvent::MessagePart {
            run_id: "run-3".into(),
            response_id: "response".into(),
            text: "second".into(),
            index: 1,
            total: 2,
        });
        for state in [&mut old, &mut new] {
            let assembled = state
                .shared_feed
                .parts
                .accept(&continuation)
                .unwrap()
                .into_owned();
            state.apply_conversation_event(&assembled);
        }
        assert_same(&old, &new);
        assert!(new.transcripts.run("run-3").unwrap().entries().iter().any(
            |entry| matches!(entry, TranscriptEntry::Message { text, .. } if text == "first second")
        ));
    }
    drop(storage);
}

#[test]
fn empty_restore_matches_legacy() {
    let db = Database::open_in_memory().unwrap();
    let mut old = state(SidebarState::default());
    let mut new = state(SidebarState::default());
    old.restore_history_legacy(&db, None).unwrap();
    new.restore_history(&db).unwrap();
    assert_same(&old, &new);
}

#[test]
fn non_repair_candidates_never_decode_their_conversation_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", dir.path())
        .unwrap();
    sidebar.select_project(&project).unwrap();
    for run in ["complete", "no-waiting", "error"] {
        sidebar
            .create_thread(ThreadId::new(run), project.clone(), run)
            .unwrap();
        sidebar.threads.last_mut().unwrap().run_ids.push(run.into());
        let mut record = context(run, run, serde_json::json!([]));
        if run == "error" {
            record.terminal_phase = "Error".into();
        }
        storage.handle().upsert_run_context(&record).unwrap();
    }
    for event in [
        started("complete", None, "chat:Worker:complete"),
        request("complete"),
        Event::new(MessageEvent::MessageCompleted {
            run_id: "complete".into(),
            text: "already complete".into(),
        }),
        phase("complete", AgentRunPhase::Waiting),
        started("no-waiting", None, "chat:Worker:no-waiting"),
        started("error", None, "chat:Worker:error"),
        phase("error", AgentRunPhase::Waiting),
    ] {
        storage.handle().append_event(Some("gui"), &event).unwrap();
    }
    let conn = rusqlite::Connection::open(&config.db_path).unwrap();
    conn.execute("UPDATE run_contexts SET messages_json=x'00'", [])
        .unwrap();
    let db = Database::open(&config).unwrap();
    // This codec poison fails on every attempted payload decode. All three
    // non-candidate paths must restore without ever attempting that decode.
    assert!(db.checkpoint_run_messages("complete").is_err());
    assert!(db.checkpoint_run_messages("no-waiting").is_err());
    assert_eq!(db.checkpoint_run_messages("error").unwrap(), None);
    state(sidebar).restore_history(&db).unwrap();
}
