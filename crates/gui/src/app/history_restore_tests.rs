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
    // Rows hold no clock reading once the history is finished, so they compare.
    let telemetry_runs: Vec<_> = old.telemetry.run_ids().collect();
    assert_eq!(telemetry_runs, new.telemetry.run_ids().collect::<Vec<_>>());
    for run in telemetry_runs {
        assert_eq!(
            old.telemetry.row(run),
            new.telemetry.row(run),
            "telemetry of {run}"
        );
    }
    assert_eq!(old.thread_goals, new.thread_goals);
    assert_eq!(old.thread_todos, new.thread_todos);
    assert_eq!(format!("{:?}", old.ledger), format!("{:?}", new.ledger));
}

#[test]
fn optimized_restore_matches_legacy_with_repairs_branches_escalation_and_feed_parts() {
    assert_restore_matches_legacy(|_| {});
}

/// Without the event columns the first pass cannot tell rows apart by
/// `subkind`, so it decodes every one; the restored state must not differ.
#[test]
fn restore_with_unfilled_event_columns_matches_legacy() {
    assert_restore_matches_legacy(|conn| {
        conn.execute("UPDATE events SET subkind = NULL, run_id = NULL", [])
            .unwrap();
    });
}

/// Rows the replay cannot read are skipped by the oracle in both of its
/// passes; the restore must skip the same rows and lose nothing else.
#[test]
fn restore_skips_unreadable_events_like_legacy() {
    assert_restore_matches_legacy(|conn| {
        let damaged = conn
            .execute(
                "UPDATE events SET payload = '{not json' \
                 WHERE subkind IN ('ToolStarted', 'ReasoningDelta')",
                [],
            )
            .unwrap();
        assert_eq!(damaged, 2);
    });
}

/// What the user sees must not change when the database the history was saved in goes
/// through `convert-bodies`: the same threads, transcripts, repairs and feed parts.
#[test]
fn restore_after_the_database_was_converted_shows_the_same_history() {
    assert_restore_matches_legacy_inner(|_| {}, true);
}

fn assert_restore_matches_legacy(
    damage: impl FnOnce(&rusqlite::Connection),
) -> WorkbenchState<DemoSource> {
    assert_restore_matches_legacy_inner(damage, false)
}

fn assert_restore_matches_legacy_inner(
    damage: impl FnOnce(&rusqlite::Connection),
    convert: bool,
) -> WorkbenchState<DemoSource> {
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
        // The long strings of the large snapshots are content blobs; small ones have none.
        let blobs: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM content_blob_refs_message r \
                 JOIN run_messages m ON m.id = r.message_id WHERE m.run_id=?1",
                [run],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(blobs > 0, expected == "blob", "{run}");
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
    // Timestamps are deliberately equal and fixed: row ID order is the replay
    // contract, and replay must not read the wall clock for stored events.
    for event in &mut events {
        event.meta.wall_clock = std::time::SystemTime::UNIX_EPOCH;
    }
    for event in &events {
        storage.handle().append_event(Some("gui"), event).unwrap();
    }
    damage(&rusqlite::Connection::open(&config.db_path).unwrap());
    let (storage, db) = if convert {
        let restore = |db: &Database| {
            let mut restored = state(sidebar.clone());
            restored.history.push(
                serde_json::from_value(serde_json::json!({
                    "thread_id":"repair", "text":"user request",
                    "at":{"secs_since_epoch":0,"nanos_since_epoch":0}
                }))
                .unwrap(),
            );
            restored.restore_history(db).unwrap();
            restored
        };
        let before = restore(&db);
        drop(connection);
        drop(db);
        storage.close();
        storage::maintenance::downgrade_to_v39(&config.db_path).unwrap();
        let summary = storage::maintenance::convert_bodies(
            &config.db_path,
            &storage::maintenance::ConvertBodiesOptions {
                apply: true,
                backup: None,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(summary.converted_runs, 6);
        let storage = Storage::open(config.clone()).unwrap();
        let db = Database::open(&config).unwrap();
        assert_same(&before, &restore(&db));
        (storage, db)
    } else {
        (storage, db)
    };
    let registry_path = dir.path().join("owners.db");
    drop(runtime::ownership::Registry::open(&registry_path).unwrap());
    let mut restored = None;
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
        // The escalated thread is dated by its event, not by when the replay ran.
        assert!(new.sidebar.threads.iter().any(|thread| {
            thread.escalation_source_run_id.as_deref() == Some("run-1") && thread.created_at == 0
        }));
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
        restored = Some(new);
    }
    drop(storage);
    restored.expect("restored at least once")
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
        let mut record = context(
            run,
            run,
            serde_json::json!([{"role": "user", "content": [{"type": "text", "text": "payload"}]}]),
        );
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
    conn.execute("UPDATE run_messages SET codec=0, skeleton=x'00'", [])
        .unwrap();
    let db = Database::open(&config).unwrap();
    // This codec poison fails on every attempted payload decode. All three
    // non-candidate paths must restore without ever attempting that decode.
    assert!(db.checkpoint_run_messages("complete").is_err());
    assert!(db.checkpoint_run_messages("no-waiting").is_err());
    assert_eq!(db.checkpoint_run_messages("error").unwrap(), None);
    state(sidebar).restore_history(&db).unwrap();
}

fn task_history() -> Vec<Event> {
    let at = |seconds: u64, mut event: Event| {
        event.meta.wall_clock = std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds);
        event
    };
    vec![
        at(10, started("run-1", None, "root")),
        at(20, started("run-11", Some("run-1"), "first child")),
        at(21, started("run-12", Some("run-1"), "second child")),
        at(22, started("run-13", Some("run-1"), "third child")),
        at(30, phase("run-11", AgentRunPhase::Done)),
        at(
            31,
            Event::new(LifecycleEvent::AgentRunStateChanged {
                run_id: "run-12".into(),
                from: AgentRunPhase::Running,
                to: AgentRunPhase::Error,
                reason: Some(event_bus::CANCELLED_REASON.into()),
            }),
        ),
        // Updates the oldest child after the others finished, but before run-13 waits.
        at(
            40,
            Event::new(LifecycleEvent::TurnCompleted {
                run_id: "run-11".into(),
                context_len: 3,
            }),
        ),
        at(41, request("run-13")),
        at(
            50,
            Event::new(MessageEvent::MessagePart {
                run_id: "run-13".into(),
                response_id: "r".into(),
                text: "a".into(),
                index: 0,
                total: 2,
            }),
        ),
        at(60, phase("run-13", AgentRunPhase::Waiting)),
        at(
            70,
            Event::new(MessageEvent::MessagePart {
                run_id: "run-13".into(),
                response_id: "r".into(),
                text: "b".into(),
                index: 1,
                total: 2,
            }),
        ),
        at(5, phase("run-1", AgentRunPhase::Waiting)),
    ]
}

/// Task rows, their order and cancellation mark come from the lifecycle
/// events alone; a first pass that skipped one would show in the task list.
#[test]
fn restored_task_rows_match_legacy_whichever_pass_reads_the_history() {
    for fill_columns in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("events.db"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        for event in task_history() {
            storage.handle().append_event(Some("gui"), &event).unwrap();
        }
        if !fill_columns {
            rusqlite::Connection::open(&config.db_path)
                .unwrap()
                .execute("UPDATE events SET subkind = NULL, run_id = NULL", [])
                .unwrap();
        }
        let db = Database::open(&config).unwrap();
        let mut old = state(SidebarState::default());
        let mut new = state(SidebarState::default());
        old.restore_history_legacy(&db, None).unwrap();
        new.restore_history(&db).unwrap();

        let rows = |state: &WorkbenchState<DemoSource>| state.tasks.rows().to_vec();
        assert_eq!(rows(&old), rows(&new), "columns filled: {fill_columns}");
        let ids: Vec<_> = rows(&new)
            .iter()
            .map(|row| row.run_id.to_string())
            .collect();
        assert_eq!(ids, ["run-13", "run-11", "run-12"]);
        assert!(rows(&new).iter().any(|row| row.cancelled));
        assert_eq!(
            old.telemetry.run_ids().collect::<Vec<_>>(),
            new.telemetry.run_ids().collect::<Vec<_>>()
        );
        assert_same(&old, &new);
        drop(storage);
    }
}

/// A completion event the replay cannot read does not count as a completion,
/// so the saved snapshot repairs the wait after it exactly as it did when the
/// first pass read the event itself.
#[test]
fn a_wait_after_an_unreadable_completion_is_still_repaired_from_the_snapshot() {
    let restored = assert_restore_matches_legacy(|conn| {
        let damaged = conn
            .execute(
                "UPDATE events SET payload = '{not json' \
                 WHERE subkind = 'MessageCompleted' AND run_id = 'run-2'",
                [],
            )
            .unwrap();
        assert_eq!(damaged, 1);
    });
    let repaired = restored.transcripts.run("run-2").unwrap().entries().iter().any(
        |entry| matches!(entry, TranscriptEntry::Message { text, .. } if text == "must not duplicate"),
    );
    assert!(repaired, "the snapshot's answer shows where the wait is");
}

fn damaged_tool_history() -> Vec<Event> {
    let mut events = vec![started("run-1", None, "chat:Worker:one"), request("run-1")];
    for n in 0..13 {
        events.push(Event::new(ToolEvent::ToolStarted {
            run_id: Some("run-1".into()),
            tool_name: "shell".into(),
            call_id: format!("call-{n}"),
            input: None,
            parent_call_id: None,
        }));
    }
    events.push(Event::new(ToolEvent::ToolStarted {
        run_id: Some("deleted".into()),
        tool_name: "shell".into(),
        call_id: "gone".into(),
        input: None,
        parent_call_id: None,
    }));
    events.push(phase("run-1", AgentRunPhase::Waiting));
    events
}

/// Every event that cannot be read stays reachable by ID, however many there
/// are, once per event although both passes meet it; a deleted run's events
/// never come back as failures; and none of it stops the restore.
#[test]
fn unreadable_events_are_all_kept_by_id_once_and_never_for_deleted_runs() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    for event in damaged_tool_history() {
        storage.handle().append_event(Some("gui"), &event).unwrap();
    }
    let conn = rusqlite::Connection::open(&config.db_path).unwrap();
    // Alternate the damage: an unusable column for some, a broken payload for
    // the rest. The first pass never opens a tool payload, the replay does.
    conn.execute(
        "UPDATE events SET wall_clock_ns = 'late' WHERE subkind = 'ToolStarted' AND id % 2 = 0",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE events SET payload = '{broken' WHERE subkind = 'ToolStarted' AND id % 2 = 1",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO run_tombstones(run_id, thread_id, family_key) VALUES ('deleted', 't', 'f')",
        [],
    )
    .unwrap();
    let damaged: Vec<i64> = conn
        .prepare("SELECT id FROM events WHERE subkind = 'ToolStarted' AND run_id = 'run-1'")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(damaged.len(), 13);

    let db = Database::open(&config).unwrap();
    let mut restored = state(SidebarState::default());
    restored.restore_history(&db).unwrap();

    assert_eq!(
        restored
            .unreadable_history_events
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        damaged
    );
    assert!(
        restored
            .unreadable_history_events
            .values()
            .all(|event| event.run_id.as_deref() == Some("run-1") && !event.reason.is_empty())
    );
    assert_eq!(restored.shown_through_event, db.last_event_id().unwrap());
    drop(storage);
}
