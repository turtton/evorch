//! 現行の型で復元できない保存イベントの読み出しを検証します。

use event_bus::{
    Event, LifecycleEvent, OrchestratorEvent, ThreadGoalPhase, ThreadGoalSnapshot, ThreadGoalUsage,
    ThreadTodoItem, ThreadTodoSnapshot, ThreadTodoStatus,
};
use rusqlite::{Connection, params};
use storage::entity::TaskStatus;
use storage::{Database, Storage, StorageConfig, StoredEvent};
use tempfile::TempDir;

// #223 で削除した variant を、当時の保存形式のまま表します。
const REMOVED_SNAPSHOT: &str = r#"{"kind":"Snapshot","payload":{"run_id":"run-1","call_id":"call-1","snapshot_id":"0123456789abcdef0123456789abcdef01234567","workspace_root":"/workspace"}}"#;
const REMOVED_WORKSPACE_WAIT: &str = r#"{"kind":"Lifecycle","payload":{"kind":"WorkspaceWaitChanged","payload":{"run_id":"run-1","call_id":"call-1","waiting":{"workspace_root":"/workspace","tool_name":"shell","command":"rg task","holder":null}}}}"#;

#[derive(Clone, Default)]
struct LogBuffer(std::sync::Arc<std::sync::Mutex<String>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log buffer lock")
            .push_str(&String::from_utf8_lossy(buf));
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn goal() -> ThreadGoalSnapshot {
    ThreadGoalSnapshot {
        goal_id: "goal-1".into(),
        thread_id: "thread-1".into(),
        root_run_id: "run-1".into(),
        related_root_run_ids: Vec::new(),
        objective: "Keep the restored goal".into(),
        original_request: "Keep the restored goal".into(),
        criteria: vec!["Goal survives restore".into()],
        checks: Vec::new(),
        phase: ThreadGoalPhase::Working,
        review_enabled: false,
        checks_paused: false,
        work_stopped: false,
        epoch: 1,
        review_round: 0,
        findings: Vec::new(),
        reason: None,
        usage: ThreadGoalUsage::default(),
        max_tokens: None,
        max_review_rounds: 3,
    }
}

fn todo() -> ThreadTodoSnapshot {
    ThreadTodoSnapshot {
        list_id: "procedures-1".into(),
        thread_id: "thread-1".into(),
        revision: 1,
        items: vec![ThreadTodoItem {
            content: "Restore the procedure".into(),
            status: ThreadTodoStatus::InProgress,
        }],
    }
}

fn insert_raw(config: &StorageConfig, kind: &str, payload: &str) {
    Connection::open(&config.db_path)
        .expect("raw connection must open")
        .execute(
            "INSERT INTO events \
             (session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload) \
             VALUES ('session-1', ?1, 0, 0, ?2, ?3)",
            params![event_bus::SCHEMA_VERSION, kind, payload],
        )
        .expect("raw event row must insert");
}

fn capture_logs<T>(action: impl FnOnce() -> T) -> (T, String) {
    let buffer = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buffer.clone())
        .with_ansi(false)
        .without_time()
        .finish();
    let result = tracing::subscriber::with_default(subscriber, action);
    let logs = buffer.0.lock().expect("log buffer lock").clone();
    (result, logs)
}

fn events(stored: Vec<StoredEvent>) -> Vec<Event> {
    stored.into_iter().map(|stored| stored.event).collect()
}

#[test]
fn rows_of_removed_variants_are_skipped_with_a_warning_count() {
    // Given: a database whose event log interleaves rows of variants removed in #223
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let config = StorageConfig {
        db_path: temp_dir.path().join("removed-variants.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).expect("storage must open");
    let handle = storage.handle();
    let started = Event::new(LifecycleEvent::Started {
        session_id: "session-1".into(),
    });
    let goal = Event::new(OrchestratorEvent::ThreadGoalUpdated { snapshot: goal() });
    let todo = Event::new(OrchestratorEvent::ThreadTodoUpdated { snapshot: todo() });
    let task = Event::new(OrchestratorEvent::TaskProgressed {
        task_id: "task-1".into(),
        run_id: "run-1".into(),
        progress: serde_json::json!({"status":"running"}),
        reason: "started".into(),
    });
    for event in [&started, &goal] {
        handle
            .append_event(Some("session-1"), event)
            .expect("event must append");
    }
    insert_raw(&config, "Snapshot", REMOVED_SNAPSHOT);
    handle
        .append_event(Some("session-1"), &todo)
        .expect("todo event must append");
    insert_raw(&config, "Lifecycle", REMOVED_WORKSPACE_WAIT);
    handle
        .append_event(Some("session-1"), &task)
        .expect("task event must append");

    // When: the projection is rebuilt and the event log is read back
    handle
        .reconcile()
        .expect("reconcile must skip the removed variants");
    storage.close();
    let database = Database::open(&config).expect("database must reopen");
    let ((all, by_session), logs) = capture_logs(|| {
        (
            database
                .events_all_ordered()
                .expect("ordered events must read"),
            database
                .events_by_session("session-1")
                .expect("session events must read"),
        )
    });

    // Then: every decodable event and its projection survive, and each read reports the skip count
    let expected = vec![started, goal, todo, task];
    assert_eq!(events(all), expected);
    assert_eq!(events(by_session), expected);
    assert_eq!(logs.matches("skipped=2").count(), 2, "{logs}");
    assert!(logs.contains("skipped stored events that no longer decode"));
    let tasks = database.durable_tasks().expect("durable tasks must read");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].status, TaskStatus::Running);
    assert!(
        database
            .session("session-1")
            .expect("session must read")
            .is_some()
    );
}
