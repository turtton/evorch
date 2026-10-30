//! イベントログの追記と復元を管理します。

use std::collections::VecDeque;
use std::time::Duration;

use event_bus::{Event, EventKind};
use rusqlite::{Connection, Row, Transaction, TransactionBehavior, params};

use crate::db::{ns_to_system_time, system_time_to_ns};
use crate::entity::SecretGuard;
use crate::{HardLimits, LimitKind, StorageError};

mod accounting;
#[cfg(test)]
mod compaction_window_tests;
pub(crate) mod diagnostic;
pub(crate) mod select;
use accounting::{day_start_ns, enforce_limit};

const NANOS_PER_DAY: i64 = 86_400_000_000_000;
// 複数 run と GUI stream の交互追記を再走査なしで扱い、メモリも上限を設けます。
const ACCOUNTING_SESSION_CAPACITY: usize = 64;

/// 永続化されたイベントと採番 ID です。
#[derive(Debug, Clone, PartialEq)]
pub struct StoredEvent {
    pub id: i64,
    pub session_id: Option<String>,
    pub event: Event,
}

/// 単一 writer 接続が保持するイベント容量のキャッシュです。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EventAccounting {
    pub session_bytes: u64,
    pub day_bytes: u64,
    pub(crate) last_event_bytes: u64,
    seeded_session_id: Option<String>,
    seeded_day_start_ns: Option<i64>,
    data_version: Option<i64>,
    sessions: VecDeque<(String, u64)>,
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
    let event = crate::entity::redact_tool_event(event);
    SecretGuard::from_env().check_event_kind(&event.kind)?;
    let payload = serde_json::to_string(&event.kind)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    let plain_len = u64::try_from(payload.len())
        .map_err(|_| StorageError::OutOfRange("event payload length"))?;
    // One event's size limit bounds what a consumer must hold in memory, so it
    // is checked on the plaintext. Session and daily limits bound the database
    // file and are charged what is actually stored, as the accounting triggers do.
    enforce_limit(LimitKind::EventSize, plain_len, limits.max_event_bytes)?;
    let kind = kind_name(&event.kind);
    let subkind = event.kind.subkind_name();
    let encoded = encode_payload(kind, subkind, &payload);
    let event_len = u64::try_from(encoded.stored_len())
        .map_err(|_| StorageError::OutOfRange("event payload length"))?;

    // 他ウィンドウの writer がシードと INSERT の間に割り込まないよう、
    // 容量検査から commit まで同じ write transaction で保護します。
    let transaction = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let data_version: i64 =
        transaction.pragma_query_value(None, "data_version", |row| row.get(0))?;
    // data_version は同じ接続自身の commit では変化しません。別接続の
    // commit を検出したときだけ再シードし、共有 DB の容量制限を維持します。
    let mut next_accounting = if accounting.data_version == Some(data_version) {
        accounting.clone()
    } else {
        EventAccounting::default()
    };
    next_accounting.data_version = Some(data_version);
    if next_accounting.seeded_session_id.as_deref() != session_id {
        next_accounting.session_bytes = match session_id {
            Some(id) => match next_accounting.sessions.iter().find(|(key, _)| key == id) {
                Some((_, bytes)) => *bytes,
                None => accounting::session_event_bytes(&transaction, id)?,
            },
            None => 0,
        };
        next_accounting.seeded_session_id = session_id.map(String::from);
    }
    let day_start = day_start_ns(event.meta.wall_clock)?;
    if next_accounting.seeded_day_start_ns != Some(day_start) {
        next_accounting.day_bytes = accounting::day_event_bytes(&transaction, day_start)?;
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
    let run_id = event.kind.primary_run_id();
    let event_len_i64 =
        i64::try_from(event_len).map_err(|_| StorageError::OutOfRange("event payload length"))?;
    transaction.execute(
        "INSERT INTO events \
         (session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload, subkind, run_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            session_id,
            schema_version,
            monotonic_ns,
            wall_clock_ns,
            kind,
            encoded,
            subkind,
            run_id
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

    if let Some(id) = session_id {
        if let Some(index) = next_accounting
            .sessions
            .iter()
            .position(|(key, _)| key == id)
        {
            next_accounting.sessions.remove(index);
        } else if next_accounting.sessions.len() == ACCOUNTING_SESSION_CAPACITY {
            next_accounting.sessions.pop_front();
        }
        next_accounting
            .sessions
            .push_back((id.to_owned(), next_session_bytes));
    }
    next_accounting.session_bytes = next_session_bytes;
    next_accounting.day_bytes = next_day_bytes;
    next_accounting.last_event_bytes = event_len;
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

/// One page of events read after a cursor.
#[derive(Debug, Clone, PartialEq)]
pub struct EventPage {
    /// Decodable events of other sessions in ID order.
    pub events: Vec<StoredEvent>,
    /// Largest event ID scanned, including excluded rows and rows that no
    /// longer decode, or the requested cursor when no row followed it. The next
    /// page starts after it.
    pub last_id: i64,
    /// Rows scanned; a page shorter than the limit reached the end of the log.
    pub scanned: usize,
}

/// Up to `limit` events with `id > after_id` in ID order. Rows of
/// `exclude_session` are passed over without reading their payload.
pub fn list_after(
    conn: &Connection,
    after_id: i64,
    limit: usize,
    exclude_session: Option<&str>,
) -> Result<EventPage, StorageError> {
    let limit = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
    let mut statement = conn.prepare_cached(
        "SELECT id, session_id, schema_version, monotonic_ns, wall_clock_ns, kind, \
         CASE WHEN session_id = ?3 THEN NULL ELSE payload END \
         FROM events WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
    )?;
    let mut rows = statement.query(params![after_id, limit, exclude_session])?;
    let mut events = Vec::new();
    let mut last_id = after_id;
    let mut scanned = 0;
    while let Some(row) = rows.next()? {
        scanned += 1;
        last_id = row.get(0)?;
        if row.get_ref(6)? == rusqlite::types::ValueRef::Null {
            continue;
        }
        match row_to_event(row) {
            Ok(event) => events.push(event),
            // 現行の型で復元できない行は読み飛ばし、カーソルだけ進めます。
            Err(StorageError::Serialization(_)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(EventPage {
        events,
        last_id,
        scanned,
    })
}

/// Largest event ID, or 0 for an empty log.
pub fn last_id(conn: &Connection) -> Result<i64, StorageError> {
    Ok(
        conn.query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |row| {
            row.get(0)
        })?,
    )
}

/// Orchestrator events in ID order. Only these rows' payloads are decoded,
/// a small share of a long history.
pub fn list_orchestrator_ordered(conn: &Connection) -> Result<Vec<StoredEvent>, StorageError> {
    let mut events = Vec::new();
    select::for_each_matching(conn, &[("Orchestrator", None)], |event| events.push(event))?;
    Ok(events)
}

/// Whether any persisted thread goal snapshot was bound to `thread_id`.
pub fn thread_goal_recorded(conn: &Connection, thread_id: &str) -> Result<bool, StorageError> {
    let goals = list_events(
        conn,
        "SELECT id, session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload \
         FROM events WHERE kind = 'Orchestrator' AND instr(payload, 'ThreadGoalUpdated') > 0",
        [],
    )?;
    Ok(goals.iter().any(|stored| {
        matches!(
            &stored.event.kind,
            EventKind::Orchestrator(event_bus::OrchestratorEvent::ThreadGoalUpdated { snapshot })
                if snapshot.thread_id == thread_id
        )
    }))
}

fn list_events<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> Result<Vec<StoredEvent>, StorageError> {
    let mut events = Vec::new();
    visit_events(conn, sql, params, |event| events.push(event))?;
    Ok(events)
}

/// Every event in ID order, handed to `visit` one at a time so that a long
/// history is never held in memory at once.
pub fn for_each_ordered(
    conn: &Connection,
    visit: impl FnMut(StoredEvent),
) -> Result<(), StorageError> {
    visit_events(
        conn,
        "SELECT id, session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload \
         FROM events ORDER BY id ASC",
        [],
        visit,
    )
}

fn visit_events<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    mut visit: impl FnMut(StoredEvent),
) -> Result<(), StorageError> {
    let mut statement = conn.prepare(sql)?;
    let mut rows = statement.query(params)?;
    let mut skipped = 0_usize;
    while let Some(row) = rows.next()? {
        match row_to_event(row) {
            Ok(event) => visit(event),
            // 削除済みの variant など、現行の型で復元できない保存イベントは読み飛ばします。
            Err(StorageError::Serialization(_)) => skipped += 1,
            Err(error) => return Err(error),
        }
    }
    if skipped > 0 {
        tracing::warn!(skipped, "skipped stored events that no longer decode");
    }
    Ok(())
}

pub(crate) fn row_to_event(row: &Row<'_>) -> Result<StoredEvent, StorageError> {
    let schema_version: i64 = row.get(2)?;
    let monotonic_ns: i64 = row.get(3)?;
    let wall_clock_ns: i64 = row.get(4)?;
    let payload = crate::codec::decode(row.get_ref(6)?)?;
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

/// Whether a payload of this `events.kind` / subkind is compressed. Only kinds
/// that are bulky and are located by `kind`, `subkind` or the `run_id` column
/// qualify. Every other kind stays TEXT because SQL reads inside its payload,
/// and so do agent messages and approvals: they are attributed to a run by
/// text in the payload (sender/recipient, call ID), so compressing them would
/// make every run lookup decode every such row.
pub(crate) fn encode_payload<'a>(
    kind: &str,
    subkind: &str,
    payload: &'a str,
) -> crate::codec::Encoded<'a> {
    if compressible_payload(kind, subkind) {
        crate::codec::encode(payload)
    } else {
        crate::codec::Encoded::Plain(payload)
    }
}

pub(crate) fn compressible_payload(kind: &str, subkind: &str) -> bool {
    match kind {
        "Tool" => !select::PAYLOAD_ATTRIBUTED_SUBKINDS.contains(&subkind),
        "Message" | "compaction" => true,
        _ => false,
    }
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
        EventKind::Diagnostic(_) => "Diagnostic",
        EventKind::Ownership(_) => "Ownership",
        EventKind::Ledger(_) => "Ledger",
    }
}

#[cfg(test)]
mod columns_tests;
#[cfg(test)]
mod compression_tests;
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
                run_id: None,
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
