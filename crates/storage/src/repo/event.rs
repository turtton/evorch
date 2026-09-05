//! イベントログの追記と復元を管理します。

use std::time::Duration;

use event_bus::{Event, EventKind};
use rusqlite::{Connection, Row, params};

use crate::db::{ns_to_system_time, system_time_to_ns};
use crate::entity::SecretGuard;
use crate::{HardLimits, LimitKind, StorageError};

mod accounting;
use accounting::{day_start_ns, enforce_limit};

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
pub struct EventAccounting {
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
    if matches!(event.kind, EventKind::Usage(_)) {
        return Err(StorageError::RawUsageEventNotPersisted);
    }
    // heuristic secret guard (ADR 0008 defense-in-depth)。serialize・INSERT・
    // accounting 更新より前に拒否し、DB 行・event accounting・session bytes を
    // 変更しない。
    SecretGuard::from_env().check_event_kind(&event.kind)?;
    let payload = serde_json::to_string(&event.kind)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    let event_len = u64::try_from(payload.len())
        .map_err(|_| StorageError::OutOfRange("event payload length"))?;
    enforce_limit(LimitKind::EventSize, event_len, limits.max_event_bytes)?;

    let mut next_accounting = accounting.clone();
    if next_accounting.seeded_session_id.as_deref() != session_id {
        next_accounting.session_bytes = match session_id {
            Some(id) => accounting::session_event_bytes(conn, id)?,
            None => 0,
        };
        next_accounting.seeded_session_id = session_id.map(String::from);
    }
    let day_start = day_start_ns(event.meta.wall_clock)?;
    if next_accounting.seeded_day_start_ns != Some(day_start) {
        next_accounting.day_bytes = accounting::day_event_bytes(conn, day_start)?;
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

const fn kind_name(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::Lifecycle(_) => "Lifecycle",
        EventKind::Message(_) => "Message",
        EventKind::Tool(_) => "Tool",
        EventKind::Usage(_) => "Usage",
        EventKind::Provider(_) => "Provider",
        EventKind::Fault(_) => "Fault",
        EventKind::AgentMessage(_) => "AgentMessage",
        // issue #63 で指定された小文字の kind。
        EventKind::Compaction(_) => "compaction",
        EventKind::Orchestrator(_) => "Orchestrator",
    }
}

#[cfg(test)]
mod limits_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod secret_guard_tests {
    use std::time::UNIX_EPOCH;

    use event_bus::{
        AgentMessage, AgentMessageEvent, AgentMessageKind, DeliveryDisposition, EventMeta,
        LifecycleEvent, MessageEvent,
    };
    use tempfile::TempDir;

    use super::*;

    fn fixture() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        crate::migrations::apply_migrations(&connection).unwrap();
        connection
    }

    fn event_with(kind: EventKind) -> Event {
        Event {
            meta: EventMeta {
                schema_version: event_bus::SCHEMA_VERSION,
                monotonic: Duration::from_nanos(1),
                wall_clock: UNIX_EPOCH,
            },
            kind,
        }
    }

    #[test]
    fn append_rejects_secret_shaped_text_without_changing_db_or_accounting() {
        // Given: 既存セッションへ一件イベントが保存済みの状態
        let connection = fixture();
        connection
            .execute(
                "INSERT INTO sessions (id, status, created_at_ns, updated_at_ns) \
                 VALUES ('s1', 'running', 0, 0)",
                [],
            )
            .unwrap();
        let mut accounting = EventAccounting::default();
        let accepted = event_with(
            LifecycleEvent::Started {
                session_id: "s1".into(),
            }
            .into(),
        );
        append_event(
            &connection,
            Some("s1"),
            &accepted,
            &HardLimits::default(),
            &mut accounting,
        )
        .unwrap();
        let accounting_before = accounting.clone();
        let rows_before: i64 = connection
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        let bytes_before: i64 = connection
            .query_row(
                "SELECT total_event_bytes FROM sessions WHERE id = 's1'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        // When: secret 形状の delta を持つイベントを追記する
        let bad = event_with(
            MessageEvent::MessageDelta {
                delta: "leak sk-test-evorch-9f8e7d6c5b4a3f2e1d".into(),
            }
            .into(),
        );
        let Err(error) = append_event(
            &connection,
            Some("s1"),
            &bad,
            &HardLimits::default(),
            &mut accounting,
        ) else {
            panic!("secret-shaped delta must be rejected");
        };

        // Then: field 名付きで拒否され、DB 行・accounting・session bytes は不変
        assert!(matches!(
            error,
            StorageError::SecretDetected {
                entity: "event",
                field: "MessageDelta.delta",
                ..
            }
        ));
        assert_eq!(accounting, accounting_before);
        let rows_after: i64 = connection
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        let bytes_after: i64 = connection
            .query_row(
                "SELECT total_event_bytes FROM sessions WHERE id = 's1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows_after, rows_before);
        assert_eq!(bytes_after, bytes_before);
    }

    #[test]
    fn secret_guard_rejects_agent_message_containing_secret() {
        // Given: 公開 StorageHandle 経路と secret 形状の本文を持つ AgentMessage イベント
        let temp_dir = TempDir::new().expect("temporary directory must be created");
        let storage = crate::Storage::open(crate::StorageConfig {
            db_path: temp_dir.path().join("guard.db"),
            ..crate::StorageConfig::default()
        })
        .expect("storage must open");
        let handle = storage.handle();
        let bad = event_with(
            AgentMessageEvent::Delivered {
                message: AgentMessage {
                    message_id: "msg-1".into(),
                    sender_run_id: "run-1".into(),
                    recipient_run_id: "run-2".into(),
                    kind: AgentMessageKind::Send,
                    content: "leak sk-test-evorch-9f8e7d6c5b4a3f2e1d".into(),
                    reply_to: None,
                },
                disposition: DeliveryDisposition::Steering,
            }
            .into(),
        );

        // When: StorageHandle::append_event で追記する
        let error = handle
            .append_event(Some("s1"), &bad)
            .expect_err("secret-shaped agent message must be rejected");

        // Then: Message イベントと同じく field 名付きで fail-closed に拒否される
        assert!(matches!(
            error,
            StorageError::SecretDetected {
                entity: "event",
                field: "AgentMessage.content",
                ..
            }
        ));
    }
}
