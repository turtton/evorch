use std::time::UNIX_EPOCH;

use event_bus::{EventMeta, LifecycleEvent};

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
