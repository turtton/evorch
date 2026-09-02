use std::time::UNIX_EPOCH;

use event_bus::{
    AgentMessage, AgentMessageEvent, AgentMessageKind, DeliveryDisposition, EventMeta,
    LifecycleEvent, UsageEvent,
};

use super::*;

fn fixture() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    crate::migrations::apply_migrations(&connection).unwrap();
    connection
}

fn event(nanos: u64) -> Event {
    Event {
        meta: EventMeta {
            schema_version: 1,
            monotonic: Duration::from_nanos(nanos),
            wall_clock: UNIX_EPOCH + Duration::from_nanos(nanos),
        },
        kind: LifecycleEvent::Started {
            session_id: "s1".into(),
        }
        .into(),
    }
}

#[test]
fn append_then_list_preserves_full_event() {
    // Given: 移行済み DB と完全な時計情報を持つイベント
    let connection = fixture();
    let expected = event(42);
    let mut accounting = EventAccounting::default();

    // When: イベントを追記して全件取得する
    append_event(
        &connection,
        Some("s1"),
        &expected,
        &HardLimits::default(),
        &mut accounting,
    )
    .unwrap();
    let stored = list_by_session(&connection, "s1").unwrap();

    // Then: イベント全体が一致する
    assert_eq!(stored[0].event, expected);
}

#[test]
fn rejected_event_does_not_advance_accounting() {
    // Given: 一件だけ収まるセッション上限と既存セッション
    let connection = fixture();
    connection
        .execute(
            "INSERT INTO sessions (id, status, created_at_ns, updated_at_ns) \
             VALUES ('s1', 'running', 0, 0)",
            [],
        )
        .unwrap();
    let accepted = event(1);
    let event_len = u64::try_from(serde_json::to_string(&accepted.kind).unwrap().len()).unwrap();
    let limits = HardLimits {
        max_session_bytes: event_len,
        ..HardLimits::default()
    };
    let mut accounting = EventAccounting::default();
    append_event(&connection, Some("s1"), &accepted, &limits, &mut accounting).unwrap();
    let before = accounting.clone();

    // When: 上限を超えるイベントを追記する
    let result = append_event(&connection, Some("s1"), &event(2), &limits, &mut accounting);

    // Then: キャッシュと DB は変更されない
    assert!(matches!(
        result,
        Err(StorageError::LimitExceeded {
            limit: LimitKind::SessionSize,
            ..
        })
    ));
    assert_eq!(accounting, before);
    assert_eq!(list_all_ordered(&connection).unwrap().len(), 1);
    assert_eq!(
        connection
            .query_row(
                "SELECT total_event_bytes FROM sessions WHERE id = 's1'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        i64::try_from(event_len).unwrap()
    );
}

#[test]
fn day_start_floors_to_utc_midnight_and_rejects_overflow() {
    // Given: UTC 二日目の途中と i64 範囲外の壁時計
    let second_day = UNIX_EPOCH + Duration::from_nanos(NANOS_PER_DAY as u64 + 7);
    let overflow = UNIX_EPOCH + Duration::from_nanos(i64::MAX as u64 + 1);

    // When: 日の開始を算出する
    let start = accounting::day_start_ns(second_day).unwrap();
    let error = accounting::day_start_ns(overflow).unwrap_err();

    // Then: UTC 深夜へ切り捨て、範囲外は拒否する
    assert_eq!(start, NANOS_PER_DAY);
    assert_eq!(error, StorageError::OutOfRange("wall_clock nanoseconds"));
}

#[test]
fn repo_append_event_rejects_raw_usage_without_increasing_row_count() {
    // Given: 移行済みDBとraw usage event
    let connection = fixture();
    let before: i64 = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .expect("events row count must be readable");
    let usage = Event {
        meta: EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::from_nanos(1),
            wall_clock: UNIX_EPOCH + Duration::from_nanos(1),
        },
        kind: UsageEvent::Usage {
            provider: "provider".into(),
            model: "model".into(),
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 30,
            cache_write_tokens: 40,
        }
        .into(),
    };
    let mut accounting = EventAccounting::default();

    // When: repo関数を直接呼び出す
    let error = append_event(
        &connection,
        None,
        &usage,
        &HardLimits::default(),
        &mut accounting,
    )
    .expect_err("repo must reject raw usage event");

    // Then: actionable errorを返しINSERTは一件も行われない
    assert_eq!(error, StorageError::RawUsageEventNotPersisted);
    let message = error.to_string();
    assert!(message.contains("raw usage events are not persisted"));
    assert!(message.contains("UsageSink"));
    let after: i64 = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .expect("events row count must be readable");
    assert_eq!(after, before);
}

#[test]
fn kind_name_maps_agent_message_events() {
    // Given: 移行済み DB と AgentMessage イベント
    let connection = fixture();
    let delivered = Event {
        meta: EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::from_nanos(1),
            wall_clock: UNIX_EPOCH + Duration::from_nanos(1),
        },
        kind: AgentMessageEvent::Delivered {
            message: AgentMessage {
                message_id: "msg-1".into(),
                sender_run_id: "run-1".into(),
                recipient_run_id: "run-2".into(),
                kind: AgentMessageKind::Send,
                content: "ping".into(),
                reply_to: None,
            },
            disposition: DeliveryDisposition::Wake,
        }
        .into(),
    };
    let mut accounting = EventAccounting::default();

    // When: イベントを追記して kind 列と復元結果を読む
    append_event(
        &connection,
        Some("s1"),
        &delivered,
        &HardLimits::default(),
        &mut accounting,
    )
    .unwrap();
    let kind: String = connection
        .query_row("SELECT kind FROM events ORDER BY id ASC", [], |row| {
            row.get(0)
        })
        .unwrap();
    let stored = list_by_session(&connection, "s1").unwrap();

    // Then: kind 列は "AgentMessage" で、復元されたイベントも同一種別である
    assert_eq!(kind, "AgentMessage");
    assert_eq!(stored[0].event, delivered);
}
