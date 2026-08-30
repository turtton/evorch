use std::time::{Duration, UNIX_EPOCH};

use event_bus::{BucketKey, Event, EventMeta, LifecycleEvent, UsageBucket};

use super::{agent_run, event, message, metrics, session, task};
use crate::entity::{
    AgentRunRecord, AgentRunStatus, MessageRecord, MessageRole, SessionRecord, SessionStatus,
    TaskRecord, TaskStatus,
};
use crate::{Database, HardLimits};

#[test]
fn repository_write_paths_accept_only_typed_records() {
    // Given: ADR 0008 credential 非永続化を守る型付きレコード
    let database = Database::open_in_memory().expect("database must open");
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

    // When: repo 書き込み経路を型付きfixtureで呼び出す
    session::create(&database.conn, &session_record).expect("session must write");
    task::create(&database.conn, &task_record).expect("task must write");
    message::create(&database.conn, &message_record).expect("message must write");
    agent_run::create(&database.conn, &run_record).expect("agent run must write");
    event::append_event(
        &database.conn,
        Some("s"),
        &event_value,
        &HardLimits::default(),
        &mut event::EventAccounting::default(),
    )
    .expect("event must write");
    metrics::upsert_buckets(&database.conn, std::slice::from_ref(&bucket))
        .expect("metrics must write");

    // Then: 全repo経路が型検査され実DBへ書き込める
}
