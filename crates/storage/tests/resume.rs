//! イベントログからのセッション復元と再調整を検証します。

#[rustfmt::skip]
mod resume {
use std::time::{Duration, UNIX_EPOCH};
use event_bus::{Event, EventMeta, LifecycleEvent, MessageEvent, ToolEvent};
use rusqlite::Connection;
use storage::entity::{SessionRecord, SessionStatus, TaskRecord, TaskStatus};
use storage::{Database, ReconcileSummary, SessionSnapshot, Storage, StorageConfig, StorageHandle};
use tempfile::TempDir;

fn config(temp: &TempDir) -> StorageConfig { StorageConfig { db_path: temp.path().join("resume.db"), ..StorageConfig::default() } }
fn open(temp: &TempDir) -> (StorageConfig, Storage, StorageHandle) { let config = config(temp); let storage = Storage::open(config.clone()).expect("storage must open"); let handle = storage.handle(); (config, storage, handle) }
fn event(kind: impl Into<event_bus::EventKind>, seconds: u64) -> Event { Event { meta: EventMeta { schema_version: event_bus::SCHEMA_VERSION, monotonic: Duration::from_secs(seconds), wall_clock: UNIX_EPOCH + Duration::from_secs(seconds) }, kind: kind.into() } }
fn append(handle: &StorageHandle, session: Option<&str>, value: &Event) { handle.append_event(session, value).expect("event must append"); }
fn started() -> LifecycleEvent { LifecycleEvent::Started { session_id: "s1".into() } }
fn running() -> SessionSnapshot { SessionSnapshot { session_id: "s1".into(), status: SessionStatus::Running, failure_reason: None, delegated_to: None, pending_message: String::new(), pending_reasoning: String::new(), open_tool_calls: Vec::new(), task_ids: Vec::new() } }
fn total_event_bytes(conn: &Connection, session_id: &str) -> u64 {
    let total: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(OCTET_LENGTH(payload)), 0) FROM events WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .expect("session payload total must read");
    u64::try_from(total).expect("total must be nonnegative")
}

#[test]
fn interrupted_session_restores_pending_output_and_open_tool_call() {
    /* Given: 応答・推論・未完了ツールを持つセッション */ let temp = TempDir::new().unwrap(); let (config, storage, handle) = open(&temp); append(&handle, Some("s1"), &event(started(), 1)); append(&handle, Some("s1"), &event(MessageEvent::MessageDelta { delta: "Hello".into() }, 2)); append(&handle, Some("s1"), &event(MessageEvent::ReasoningDelta { delta: "think".into() }, 3)); append(&handle, Some("s1"), &event(ToolEvent::ToolStarted { tool_name: "tool".into(), call_id: "c1".into() }, 4)); storage.close(); let db = Database::open(&config).unwrap();
    /* When: セッションを復元する */ let actual = db.restore_session("s1").unwrap();
    /* Then: 保留状態を全て復元する */ assert_eq!(actual, Some(SessionSnapshot { pending_message: "Hello".into(), pending_reasoning: "think".into(), open_tool_calls: vec![("tool".into(), "c1".into())], ..running() }));
}

#[test]
fn delegated_session_restores_target() {
    /* Given: 委譲イベント */ let temp = TempDir::new().unwrap(); let (config, storage, handle) = open(&temp); append(&handle, Some("s1"), &event(LifecycleEvent::Delegated { session_id: "s1".into(), target: "agent-a".into() }, 1)); storage.close(); let db = Database::open(&config).unwrap();
    /* When: 復元する */ let actual = db.restore_session("s1").unwrap();
    /* Then: 委譲先を復元する */ assert_eq!(actual, Some(SessionSnapshot { status: SessionStatus::Delegated, delegated_to: Some("agent-a".into()), ..running() }));
}

#[test]
fn completed_session_restores_terminal_status() {
    /* Given: 開始後に完了したセッション */ let temp = TempDir::new().unwrap(); let (config, storage, handle) = open(&temp); append(&handle, Some("s1"), &event(started(), 1)); append(&handle, Some("s1"), &event(LifecycleEvent::Completed { session_id: "s1".into() }, 2)); storage.close(); let db = Database::open(&config).unwrap();
    /* When: 復元する */ let actual = db.restore_session("s1").unwrap();
    /* Then: 完了状態を復元する */ assert_eq!(actual, Some(SessionSnapshot { status: SessionStatus::Completed, ..running() }));
}

#[test]
fn failed_session_restores_reason() {
    /* Given: 失敗イベント */ let temp = TempDir::new().unwrap(); let (config, storage, handle) = open(&temp); append(&handle, Some("s1"), &event(LifecycleEvent::Failed { session_id: "s1".into(), reason: "boom".into() }, 1)); storage.close(); let db = Database::open(&config).unwrap();
    /* When: 復元する */ let actual = db.restore_session("s1").unwrap();
    /* Then: 失敗理由を復元する */ assert_eq!(actual, Some(SessionSnapshot { status: SessionStatus::Failed, failure_reason: Some("boom".into()), ..running() }));
}

#[test]
fn background_task_reconciles_completed_task_and_session_rows() {
    /* Given: 完了済みタスクを持つセッション */ let temp = TempDir::new().unwrap(); let (config, storage, handle) = open(&temp); append(&handle, Some("s1"), &event(started(), 1)); append(&handle, Some("s1"), &event(LifecycleEvent::BackgroundTaskStarted { task_id: "t1".into() }, 2)); append(&handle, Some("s1"), &event(LifecycleEvent::BackgroundTaskCompleted { task_id: "t1".into() }, 3));
    /* When: 復元して再調整する */ let summary = handle.reconcile().unwrap(); storage.close(); let db = Database::open(&config).unwrap(); let snapshot = db.restore_session("s1").unwrap(); let raw = Connection::open(&config.db_path).unwrap();
    /* Then: 全フィールドをイベント状態へ揃える */ assert_eq!(snapshot, Some(SessionSnapshot { task_ids: vec!["t1".into()], ..running() })); assert_eq!(summary, ReconcileSummary { sessions_upserted: 1, tasks_upserted: 1 }); assert_eq!(db.session("s1").unwrap(), Some(SessionRecord { id: "s1".into(), parent_id: None, status: SessionStatus::Running, failure_reason: None, delegated_to: None, total_event_bytes: total_event_bytes(&raw, "s1"), created_at: UNIX_EPOCH + Duration::from_secs(1), updated_at: UNIX_EPOCH + Duration::from_secs(3) })); assert_eq!(db.task("t1").unwrap(), Some(TaskRecord { id: "t1".into(), session_id: Some("s1".into()), status: TaskStatus::Completed, created_at: UNIX_EPOCH + Duration::from_secs(2), updated_at: UNIX_EPOCH + Duration::from_secs(3) }));
}

#[test]
fn unattributed_message_is_skipped_by_restore_and_reconcile() {
    /* Given: 帰属のないメッセージ */ let temp = TempDir::new().unwrap(); let (config, storage, handle) = open(&temp); append(&handle, None, &event(MessageEvent::MessageDelta { delta: "orphan".into() }, 1));
    /* When: 復元して再調整する */ let summary = handle.reconcile().unwrap(); storage.close(); let db = Database::open(&config).unwrap(); let restored = db.restore_sessions().unwrap();
    /* Then: セッション行を作らない */ assert_eq!(restored, Vec::<SessionSnapshot>::new()); assert_eq!(summary, ReconcileSummary { sessions_upserted: 0, tasks_upserted: 0 }); assert_eq!(db.session("s1").unwrap(), None);
}

#[test]
fn replay_uses_stored_id_order_instead_of_wall_clock_order() {
    /* Given: 壁時計と ID 順が逆のイベント */ let temp = TempDir::new().unwrap(); let (config, storage, handle) = open(&temp); append(&handle, Some("s1"), &event(started(), 20)); append(&handle, Some("s1"), &event(LifecycleEvent::Failed { session_id: "s1".into(), reason: "last-id".into() }, 10)); storage.close(); let db = Database::open(&config).unwrap();
    /* When: 復元する */ let actual = db.restore_session("s1").unwrap();
    /* Then: 最後の ID が勝つ */ assert_eq!(actual, Some(SessionSnapshot { status: SessionStatus::Failed, failure_reason: Some("last-id".into()), ..running() }));
}

#[test]
fn completed_tool_call_is_removed_from_open_calls() {
    /* Given: 開始・完了したツール呼び出し */ let temp = TempDir::new().unwrap(); let (config, storage, handle) = open(&temp); append(&handle, Some("s1"), &event(started(), 1)); append(&handle, Some("s1"), &event(ToolEvent::ToolStarted { tool_name: "tool".into(), call_id: "c1".into() }, 2)); append(&handle, Some("s1"), &event(ToolEvent::ToolCompleted { tool_name: "tool".into(), call_id: "c1".into(), is_error: false, detail: None }, 3)); storage.close(); let db = Database::open(&config).unwrap();
    /* When: 復元する */ let actual = db.restore_session("s1").unwrap();
    /* Then: 未完了一覧が空になる */ assert_eq!(actual, Some(running()));
}

#[test]
fn reconcile_is_idempotent_and_reports_stable_counts() {
    /* Given: 完了済みタスクのログ */ let temp = TempDir::new().unwrap(); let (config, storage, handle) = open(&temp); append(&handle, Some("s1"), &event(started(), 1)); append(&handle, Some("s1"), &event(LifecycleEvent::BackgroundTaskStarted { task_id: "t1".into() }, 2)); append(&handle, Some("s1"), &event(LifecycleEvent::BackgroundTaskCompleted { task_id: "t1".into() }, 3));
    /* When: 二度再調整する */ let first = handle.reconcile().unwrap(); let reader = Database::open(&config).expect("reader must open while writer runs"); let first_session = reader.session("s1").unwrap(); let first_task = reader.task("t1").unwrap(); drop(reader); let second = handle.reconcile().unwrap(); storage.close(); let db = Database::open(&config).unwrap();
    /* Then: 件数と行が変わらない */ assert_eq!(second, first); assert_eq!(db.session("s1").unwrap(), first_session); assert_eq!(db.task("t1").unwrap(), first_task);
}
}
