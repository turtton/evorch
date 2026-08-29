//! イベントログの容量制限と復元性を検証します。

use std::time::{Duration, UNIX_EPOCH};

use event_bus::{Event, EventMeta, LifecycleEvent, MessageEvent};
use rusqlite::Connection;
use storage::repo::event::{EventAccounting, append_event, list_by_session};
use storage::{Database, HardLimits, LimitKind, StorageConfig, StorageError};
use tempfile::TempDir;

fn open_connection(temp_dir: &TempDir) -> Connection {
    let path = temp_dir.path().join("limits.db");
    let config = StorageConfig {
        db_path: path.clone(),
        ..StorageConfig::default()
    };
    drop(Database::open(&config).expect("database must open"));
    Connection::open(path).expect("migrated database must reopen")
}

fn insert_session(connection: &Connection, session_id: &str) {
    connection
        .execute(
            "INSERT INTO sessions \
             (id, status, created_at_ns, updated_at_ns) VALUES (?1, 'running', 0, 0)",
            [session_id],
        )
        .expect("session fixture must insert");
}

fn event_at(kind: impl Into<event_bus::EventKind>, nanos: u64) -> Event {
    Event {
        meta: EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::from_nanos(nanos),
            wall_clock: UNIX_EPOCH + Duration::from_nanos(nanos),
        },
        kind: kind.into(),
    }
}

fn payload_len(event: &Event) -> u64 {
    u64::try_from(
        serde_json::to_string(&event.kind)
            .expect("event kind must serialize")
            .len(),
    )
    .expect("payload length must fit u64")
}

fn session_total(connection: &Connection, session_id: &str) -> u64 {
    let total: i64 = connection
        .query_row(
            "SELECT total_event_bytes FROM sessions WHERE id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .expect("session total must read");
    u64::try_from(total).expect("session total must be nonnegative")
}

#[test]
fn oversized_event_is_rejected_before_insert() {
    // Given: 既定の単一イベント上限を超えるメッセージ
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let connection = open_connection(&temp_dir);
    let event = event_at(
        MessageEvent::MessageDelta {
            delta: "x".repeat(300_000),
        },
        1,
    );
    let actual = payload_len(&event);
    let mut accounting = EventAccounting::default();

    // When: イベントを追記する
    let error = append_event(
        &connection,
        None,
        &event,
        &HardLimits::default(),
        &mut accounting,
    )
    .expect_err("oversized event must fail");

    // Then: 実際の直列化サイズを伴う EventSize 超過になる
    assert_eq!(
        error,
        StorageError::LimitExceeded {
            limit: LimitKind::EventSize,
            actual,
            max: HardLimits::default().max_event_bytes,
        }
    );
}

#[test]
fn session_budget_rejects_only_the_overflowing_event() {
    // Given: 一件だけ収まるセッション上限と永続化済みセッション
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let connection = open_connection(&temp_dir);
    insert_session(&connection, "s1");
    let first = event_at(
        MessageEvent::MessageDelta {
            delta: "a".repeat(20),
        },
        1,
    );
    let second = event_at(
        MessageEvent::MessageDelta {
            delta: "b".repeat(20),
        },
        2,
    );
    let first_len = payload_len(&first);
    let second_len = payload_len(&second);
    let limits = HardLimits {
        max_session_bytes: first_len + second_len - 1,
        ..HardLimits::default()
    };
    let mut accounting = EventAccounting::default();
    append_event(&connection, Some("s1"), &first, &limits, &mut accounting)
        .expect("first event must fit");
    let accepted_accounting = accounting.clone();

    // When: 合計が上限を超える二件目を追記する
    let error = append_event(&connection, Some("s1"), &second, &limits, &mut accounting)
        .expect_err("overflowing event must fail");

    // Then: 二件目は保存・課金されず、受理済みサイズだけが残る
    assert_eq!(
        error,
        StorageError::LimitExceeded {
            limit: LimitKind::SessionSize,
            actual: first_len + second_len,
            max: limits.max_session_bytes,
        }
    );
    assert_eq!(session_total(&connection, "s1"), first_len);
    assert_eq!(list_by_session(&connection, "s1").unwrap().len(), 1);
    assert_eq!(accounting, accepted_accounting);
}

#[test]
fn daily_budget_rejects_event_crossing_same_utc_day_limit() {
    // Given: 同じ UTC 日に一件だけ収まる日次上限
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let connection = open_connection(&temp_dir);
    let day = 86_400_u64 * 1_000_000_000;
    let first = event_at(
        MessageEvent::MessageDelta {
            delta: "a".repeat(20),
        },
        day + 1,
    );
    let second = event_at(
        MessageEvent::MessageDelta {
            delta: "b".repeat(20),
        },
        day + 2,
    );
    let total = payload_len(&first) + payload_len(&second);
    let limits = HardLimits {
        max_daily_event_bytes: total - 1,
        ..HardLimits::default()
    };
    let mut accounting = EventAccounting::default();
    append_event(&connection, None, &first, &limits, &mut accounting)
        .expect("first event must fit");

    // When: 同日の合計が上限を超える二件目を追記する
    let error = append_event(&connection, None, &second, &limits, &mut accounting)
        .expect_err("daily overflow must fail");

    // Then: DailyBytes 超過として拒否される
    assert_eq!(
        error,
        StorageError::LimitExceeded {
            limit: LimitKind::DailyBytes,
            actual: total,
            max: limits.max_daily_event_bytes,
        }
    );
}

#[test]
fn accepted_events_round_trip_with_exact_timestamps() {
    // Given: ナノ秒精度の時計を持つ二種類のイベント
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let connection = open_connection(&temp_dir);
    insert_session(&connection, "s1");
    let events = [
        event_at(
            LifecycleEvent::Started {
                session_id: "s1".into(),
            },
            123,
        ),
        event_at(
            MessageEvent::MessageDelta {
                delta: "hello".into(),
            },
            456,
        ),
    ];
    let mut accounting = EventAccounting::default();

    // When: 同じセッションへ順に追記して一覧を取得する
    for event in &events {
        append_event(
            &connection,
            Some("s1"),
            event,
            &HardLimits::default(),
            &mut accounting,
        )
        .expect("event must append");
    }
    let stored = list_by_session(&connection, "s1").expect("events must list");

    // Then: ID 順でイベント全体が完全に復元される
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0].event, events[0]);
    assert_eq!(stored[1].event, events[1]);
}

#[test]
fn event_without_session_appends_without_foreign_key_target() {
    // Given: セッションに属さないイベント
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let connection = open_connection(&temp_dir);
    let event = event_at(
        LifecycleEvent::Completed {
            session_id: "gone".into(),
        },
        7,
    );
    let mut accounting = EventAccounting::default();

    // When: session_id なしで追記する
    let stored = append_event(
        &connection,
        None,
        &event,
        &HardLimits::default(),
        &mut accounting,
    )
    .expect("append-first event must succeed");

    // Then: セッション参照なしでイベントを保持する
    assert_eq!(stored.session_id, None);
    assert_eq!(stored.event, event);
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM events", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}
