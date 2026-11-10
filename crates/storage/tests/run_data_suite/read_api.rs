//! Database の公開 read-only facade を検証します。

use rusqlite::{Connection, params};
use storage::{Database, Storage, StorageConfig};
use tempfile::TempDir;

#[test]
fn database_read_facade_roundtrips_all_entity_queries() {
    // Given: 全 read API の対象行を持つファイル DB
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let config = StorageConfig {
        db_path: temp_dir.path().join("read-api.db"),
        ..StorageConfig::default()
    };
    drop(Database::open(&config).expect("database must open"));
    let connection = Connection::open(&config.db_path).expect("database must reopen");
    connection
        .pragma_update(None, "foreign_keys", true)
        .unwrap();
    connection.execute("INSERT INTO sessions (id,parent_id,status,total_event_bytes,created_at_ns,updated_at_ns) VALUES ('parent',NULL,'running',0,1,1),('s1','parent','running',0,2,2)", []).unwrap();
    connection.execute("INSERT INTO tasks (id,session_id,status,created_at_ns,updated_at_ns) VALUES ('t1','s1','running',3,3)", []).unwrap();
    connection.execute("INSERT INTO messages (id,session_id,role,content,reasoning,created_at_ns,updated_at_ns) VALUES ('m1','s1','user','hello',NULL,4,4)", []).unwrap();
    connection.execute("INSERT INTO agent_runs (id,session_id,provider,model,status,started_at_ns,finished_at_ns) VALUES ('r1','s1','p','m','running',5,NULL)", []).unwrap();
    let event = event_bus::Event::new(event_bus::LifecycleEvent::Started {
        session_id: "s1".into(),
    });
    connection.execute("INSERT INTO events (session_id,schema_version,monotonic_ns,wall_clock_ns,kind,payload) VALUES ('s1',?1,0,6,'Lifecycle',?2)", params![i64::from(event.meta.schema_version), serde_json::to_string(&event.kind).unwrap()]).unwrap();
    connection.execute("INSERT INTO downsampled_metrics (window_start,provider,model,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,cache_hits,cache_misses,request_count) VALUES (60,'p','m',1,2,3,4,5,6,1)", []).unwrap();
    drop(connection);
    let database = Database::open(&config).expect("database must reopen");

    // When: 公開 facade の全 read API を呼ぶ
    let session = database.session("s1").unwrap();
    let sessions = database.sessions_by_parent("parent").unwrap();
    let task = database.task("t1").unwrap();
    let tasks = database.tasks_by_session("s1").unwrap();
    let message = database.message("m1").unwrap();
    let messages = database.messages_by_session("s1").unwrap();
    let run = database.agent_run("r1").unwrap();
    let runs = database.agent_runs_by_session("s1").unwrap();
    let events = database.events_by_session("s1").unwrap();
    let all_events = database.events_all_ordered().unwrap();
    let metrics = database.metrics_range(0, 60).unwrap();
    let restored = database.restore_session("s1").unwrap();
    let restored_all = database.restore_sessions().unwrap();

    // Then: 各問い合わせが seed した対象を返す
    assert_eq!(session.unwrap().id, "s1");
    assert_eq!(sessions[0].id, "s1");
    assert_eq!(sessions.len(), 1);
    assert_eq!(task.unwrap().id, "t1");
    assert_eq!(tasks[0].id, "t1");
    assert_eq!(tasks.len(), 1);
    assert_eq!(message.unwrap().id, "m1");
    assert_eq!(messages[0].id, "m1");
    assert_eq!(messages[0].content, "hello");
    assert_eq!(messages.len(), 1);
    assert_eq!(run.unwrap().id, "r1");
    assert_eq!(runs[0].id, "r1");
    assert_eq!(runs.len(), 1);
    assert_eq!(events[0].event.kind, event.kind);
    assert_eq!(events.len(), 1);
    assert_eq!(all_events, events);
    assert_eq!(metrics.len(), 1);
    assert_eq!(metrics[0].key.provider, "p");
    assert_eq!(metrics[0].key.model, "m");
    assert_eq!(metrics[0].input_tokens, 1);
    assert_eq!(metrics[0].output_tokens, 2);
    assert_eq!(restored.unwrap().session_id, "s1");
    assert_eq!(restored_all.len(), 1);
    assert_eq!(restored_all[0].session_id, "s1");
}

#[test]
fn orchestrator_events_round_trip_through_sqlite() {
    // Given: a file-backed storage writer and two orchestrator events
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let config = StorageConfig {
        db_path: temp_dir.path().join("orchestrator-events.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).expect("storage must open");
    let handle = storage.handle();
    let created = event_bus::Event::new(event_bus::OrchestratorEvent::TaskProgressed {
        task_id: "task-1".into(),
        run_id: "run-1".into(),
        progress: serde_json::json!({"completed": ["ac1"]}),
        reason: "criterion completed".into(),
    });
    let changed = event_bus::Event::new(event_bus::OrchestratorEvent::TaskCheckpoint {
        task_id: "task-1".into(),
        run_id: "run-1".into(),
        tool_call_count: 1,
        cumulative_input_tokens: 2,
        cumulative_output_tokens: 3,
        elapsed_ms: 4,
    });

    // When: append through the public writer handle and reopen the database
    handle
        .append_event(Some("session-1"), &created)
        .expect("created event must append");
    handle
        .append_event(Some("session-1"), &changed)
        .expect("checkpoint event must append");
    drop(storage);
    let database = Database::open(&config).expect("database must reopen");

    // Then: the ordered read API restores both events exactly
    let events: Vec<_> = database
        .events_all_ordered()
        .expect("events must read")
        .into_iter()
        .filter(|stored| matches!(stored.event.kind, event_bus::EventKind::Orchestrator(_)))
        .map(|stored| stored.event)
        .collect();
    assert_eq!(events, vec![created, changed]);
}

#[test]
fn orchestrator_read_returns_only_orchestrator_events_in_order() {
    // Given: orchestrator events interleaved with other kinds
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let config = StorageConfig {
        db_path: temp_dir.path().join("orchestrator-only.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).expect("storage must open");
    let handle = storage.handle();
    let progressed = event_bus::Event::new(event_bus::OrchestratorEvent::TaskProgressed {
        task_id: "task-1".into(),
        run_id: "run-1".into(),
        progress: serde_json::json!({"completed": ["ac1"]}),
        reason: "criterion completed".into(),
    });
    let checkpoint = event_bus::Event::new(event_bus::OrchestratorEvent::TaskCheckpoint {
        task_id: "task-1".into(),
        run_id: "run-1".into(),
        tool_call_count: 1,
        cumulative_input_tokens: 2,
        cumulative_output_tokens: 3,
        elapsed_ms: 4,
    });
    let message = |text: &str| {
        event_bus::Event::new(event_bus::MessageEvent::MessageCompleted {
            run_id: "run-1".into(),
            text: text.into(),
        })
    };
    for event in [
        message("before"),
        progressed.clone(),
        message("between"),
        checkpoint.clone(),
        message("after"),
    ] {
        handle
            .append_event(Some("session-1"), &event)
            .expect("event must append");
    }
    drop(storage);
    let database = Database::open(&config).expect("database must reopen");

    // When: reading only orchestrator events
    let events = database
        .orchestrator_events_ordered()
        .expect("events must read");

    // Then: exactly the orchestrator events come back, in append order
    assert_eq!(
        events
            .iter()
            .map(|stored| &stored.event)
            .collect::<Vec<_>>(),
        vec![&progressed, &checkpoint]
    );
    assert!(events.windows(2).all(|pair| pair[0].id < pair[1].id));
}
