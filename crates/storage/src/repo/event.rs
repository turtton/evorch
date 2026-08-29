//! イベントログの追記と復元を管理します。
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "single-writer integration lands in the next storage task"
    )
)]

use std::time::{Duration, SystemTime};

use event_bus::{Event, EventKind};
use rusqlite::{Connection, Row, params};

use crate::db::{ns_to_system_time, system_time_to_ns};
use crate::{HardLimits, LimitKind, StorageError};

const NANOS_PER_DAY: i64 = 86_400_000_000_000;

/// 永続化されたイベントと採番 ID です。
#[derive(Debug, Clone, PartialEq)]
pub struct StoredEvent {
    pub id: i64,
    pub session_id: Option<String>,
    pub event: Event,
}

/// writer が保持するイベント容量のキャッシュです。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct EventAccounting {
    pub session_bytes: u64,
    pub day_bytes: u64,
    seeded_session_id: Option<String>,
    seeded_day_start_ns: Option<i64>,
}

/// 容量上限を検査してイベントを一件追記します。
pub fn append_event(
    conn: &Connection,
    session_id: Option<&str>,
    event: &Event,
    limits: &HardLimits,
    accounting: &mut EventAccounting,
) -> Result<StoredEvent, StorageError> {
    let payload = serde_json::to_string(&event.kind)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    let event_len = u64::try_from(payload.len())
        .map_err(|_| StorageError::OutOfRange("event payload length"))?;
    enforce_limit(LimitKind::EventSize, event_len, limits.max_event_bytes)?;

    let mut next_accounting = accounting.clone();
    if next_accounting.seeded_session_id.as_deref() != session_id {
        next_accounting.session_bytes = match session_id {
            Some(id) => session_event_bytes(conn, id)?,
            None => 0,
        };
        next_accounting.seeded_session_id = session_id.map(String::from);
    }
    let day_start = day_start_ns(event.meta.wall_clock)?;
    if next_accounting.seeded_day_start_ns != Some(day_start) {
        next_accounting.day_bytes = day_event_bytes(conn, day_start)?;
        next_accounting.seeded_day_start_ns = Some(day_start);
    }

    let next_session_bytes = next_accounting
        .session_bytes
        .checked_add(event_len)
        .ok_or(StorageError::OutOfRange("session event bytes"))?;
    if session_id.is_some() {
        enforce_limit(
            LimitKind::SessionSize,
            next_session_bytes,
            limits.max_session_bytes,
        )?;
    }
    let next_day_bytes = next_accounting
        .day_bytes
        .checked_add(event_len)
        .ok_or(StorageError::OutOfRange("daily event bytes"))?;
    enforce_limit(
        LimitKind::DailyBytes,
        next_day_bytes,
        limits.max_daily_event_bytes,
    )?;

    let monotonic_ns = i64::try_from(event.meta.monotonic.as_nanos())
        .map_err(|_| StorageError::OutOfRange("monotonic nanoseconds"))?;
    let wall_clock_ns = system_time_to_ns(event.meta.wall_clock)?;
    let schema_version = i64::from(event.meta.schema_version);
    let kind = kind_name(&event.kind);
    let event_len_i64 =
        i64::try_from(event_len).map_err(|_| StorageError::OutOfRange("event payload length"))?;
    let transaction = conn.unchecked_transaction()?;
    transaction.execute(
        "INSERT INTO events \
         (session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            session_id,
            schema_version,
            monotonic_ns,
            wall_clock_ns,
            kind,
            payload
        ],
    )?;
    let id = transaction.last_insert_rowid();
    if let Some(id) = session_id {
        transaction.execute(
            "UPDATE sessions SET total_event_bytes = total_event_bytes + ?1 WHERE id = ?2",
            params![event_len_i64, id],
        )?;
    }
    transaction.commit()?;

    next_accounting.session_bytes = next_session_bytes;
    next_accounting.day_bytes = next_day_bytes;
    *accounting = next_accounting;
    Ok(StoredEvent {
        id,
        session_id: session_id.map(String::from),
        event: event.clone(),
    })
}

/// セッションに属するイベント payload の累積バイト数を返します。
pub fn session_event_bytes(conn: &Connection, session_id: &str) -> Result<u64, StorageError> {
    sum_payload_bytes(
        conn,
        "SELECT COALESCE(SUM(LENGTH(payload)), 0) FROM events WHERE session_id = ?1",
        rusqlite::params![session_id],
    )
}

/// 指定 UTC 日の開始以降に記録された payload の累積バイト数を返します。
pub fn day_event_bytes(conn: &Connection, day_start_ns: i64) -> Result<u64, StorageError> {
    sum_payload_bytes(
        conn,
        "SELECT COALESCE(SUM(LENGTH(payload)), 0) FROM events WHERE wall_clock_ns >= ?1",
        rusqlite::params![day_start_ns],
    )
}

/// セッションのイベントを採番順で返します。
pub fn list_by_session(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<StoredEvent>, StorageError> {
    list_events(
        conn,
        "SELECT id, session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload \
         FROM events WHERE session_id = ?1 ORDER BY id ASC",
        rusqlite::params![session_id],
    )
}

/// 全イベントを採番順で返します。
pub fn list_all_ordered(conn: &Connection) -> Result<Vec<StoredEvent>, StorageError> {
    list_events(
        conn,
        "SELECT id, session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload \
         FROM events ORDER BY id ASC",
        [],
    )
}

fn list_events<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> Result<Vec<StoredEvent>, StorageError> {
    let mut statement = conn.prepare(sql)?;
    let mut rows = statement.query(params)?;
    let mut events = Vec::new();
    while let Some(row) = rows.next()? {
        events.push(row_to_event(row)?);
    }
    Ok(events)
}

pub(crate) fn row_to_event(row: &Row<'_>) -> Result<StoredEvent, StorageError> {
    let schema_version: i64 = row.get(2)?;
    let monotonic_ns: i64 = row.get(3)?;
    let wall_clock_ns: i64 = row.get(4)?;
    let payload: String = row.get(6)?;
    let event = Event {
        meta: event_bus::EventMeta {
            schema_version: u32::try_from(schema_version)
                .map_err(|_| StorageError::OutOfRange("event schema version"))?,
            monotonic: Duration::from_nanos(
                u64::try_from(monotonic_ns)
                    .map_err(|_| StorageError::OutOfRange("monotonic nanoseconds"))?,
            ),
            wall_clock: ns_to_system_time(wall_clock_ns),
        },
        kind: serde_json::from_str(&payload)
            .map_err(|error| StorageError::Serialization(error.to_string()))?,
    };
    Ok(StoredEvent {
        id: row.get(0)?,
        session_id: row.get(1)?,
        event,
    })
}

fn day_start_ns(time: SystemTime) -> Result<i64, StorageError> {
    let nanos = system_time_to_ns(time)?;
    Ok(nanos / NANOS_PER_DAY * NANOS_PER_DAY)
}

fn enforce_limit(limit: LimitKind, actual: u64, max: u64) -> Result<(), StorageError> {
    if actual > max {
        return Err(StorageError::LimitExceeded { limit, actual, max });
    }
    Ok(())
}

fn sum_payload_bytes<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> Result<u64, StorageError> {
    let total: i64 = conn.query_row(sql, params, |row| row.get(0))?;
    u64::try_from(total).map_err(|_| StorageError::OutOfRange("event payload byte total"))
}

const fn kind_name(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::Lifecycle(_) => "Lifecycle",
        EventKind::Message(_) => "Message",
        EventKind::Tool(_) => "Tool",
        EventKind::Usage(_) => "Usage",
        EventKind::Provider(_) => "Provider",
        EventKind::Fault(_) => "Fault",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_bus::{EventMeta, LifecycleEvent};
    use std::time::UNIX_EPOCH;

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
        // Given: payload より小さい単一イベント上限
        let connection = fixture();
        connection
            .execute(
                "INSERT INTO sessions (id, status, created_at_ns, updated_at_ns) \
                 VALUES ('s1', 'running', 0, 0)",
                [],
            )
            .unwrap();
        let accepted = event(1);
        let event_len =
            u64::try_from(serde_json::to_string(&accepted.kind).unwrap().len()).unwrap();
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
        let start = day_start_ns(second_day).unwrap();
        let error = day_start_ns(overflow).unwrap_err();

        // Then: UTC 深夜へ切り捨て、範囲外は拒否する
        assert_eq!(start, NANOS_PER_DAY);
        assert_eq!(error, StorageError::OutOfRange("wall_clock nanoseconds"));
    }
}
