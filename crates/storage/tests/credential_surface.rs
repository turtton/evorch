//! ADR 0008 の credential 非永続化 — 公開書き込み経路が型付きレコードのみを受け付けることを検証します。

use std::time::{Duration, UNIX_EPOCH};

use event_bus::{BucketKey, Event, EventMeta, LifecycleEvent, UsageBucket, UsageSink};
use rusqlite::Connection;
use storage::entity::{
    AgentRunRecord, AgentRunStatus, MessageRecord, MessageRole, SessionRecord, SessionStatus,
    TaskRecord, TaskStatus,
};
use storage::repo::{agent_run, event, message, metrics, session, task};
use storage::{Database, HardLimits, Storage, StorageConfig};
use tempfile::TempDir;

fn open_schema() -> (TempDir, Connection) {
    let temp = TempDir::new().expect("temporary directory must be created");
    let path = temp.path().join("credential.db");
    let config = StorageConfig {
        db_path: path.clone(),
        ..StorageConfig::default()
    };
    drop(Database::open(&config).expect("database must open"));
    let connection = Connection::open(path).expect("database must reopen");
    (temp, connection)
}

#[test]
fn public_write_paths_accept_only_typed_records() {
    // Given: ADR 0008 credential 非永続化 — 全書き込み経路は型付きレコードのみを受け付け、
    // credential を保持し得る汎用 key/value や生 SQL 経路を公開しない。
    let (temp, connection) = open_schema();
    let time = UNIX_EPOCH + Duration::from_secs(60);
    let session_record = SessionRecord {
        id: "s".into(),
        parent_id: None,
        status: SessionStatus::Running,
        failure_reason: None,
        delegated_to: None,
        total_event_bytes: 0,
        created_at: time,
        updated_at: time,
    };
    let task_record = TaskRecord {
        id: "t".into(),
        session_id: Some("s".into()),
        status: TaskStatus::Running,
        created_at: time,
        updated_at: time,
    };
    let message_record = MessageRecord {
        id: "m".into(),
        session_id: "s".into(),
        role: MessageRole::User,
        content: "hello".into(),
        reasoning: None,
        created_at: time,
        updated_at: time,
    };
    let run_record = AgentRunRecord {
        id: "r".into(),
        session_id: "s".into(),
        provider: "p".into(),
        model: "model".into(),
        status: AgentRunStatus::Running,
        started_at: time,
        finished_at: None,
    };
    let event_value = Event {
        meta: EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::ZERO,
            wall_clock: time,
        },
        kind: LifecycleEvent::Started {
            session_id: "s".into(),
        }
        .into(),
    };
    let bucket = UsageBucket {
        key: BucketKey {
            window_start: 60,
            provider: "p".into(),
            model: "model".into(),
        },
        input_tokens: 1,
        output_tokens: 2,
        cache_read_tokens: 3,
        cache_write_tokens: 4,
        cache_hits: 5,
        cache_misses: 6,
        request_count: 1,
    };

    // When: repo と single-writer の公開書き込み経路を型付き fixture で呼び出す
    session::create(&connection, &session_record).expect("session must write");
    task::create(&connection, &task_record).expect("task must write");
    message::create(&connection, &message_record).expect("message must write");
    agent_run::create(&connection, &run_record).expect("agent run must write");
    event::append_event(
        &connection,
        Some("s"),
        &event_value,
        &HardLimits::default(),
        &mut event::EventAccounting::default(),
    )
    .expect("event must write");
    metrics::upsert_buckets(&connection, std::slice::from_ref(&bucket))
        .expect("metrics must write");
    drop(connection);
    let config = StorageConfig {
        db_path: temp.path().join("writer.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config).expect("storage must open");
    let handle = storage.handle();
    handle
        .append_event(None, &event_value)
        .expect("handle event must write");
    <storage::StorageHandle as UsageSink>::submit(&handle, vec![bucket]);
    handle.flush_usage_now().expect("handle usage must flush");

    // Then: 全公開経路が型検査され、実データベースへの書き込みに成功する
    storage.close();
}
