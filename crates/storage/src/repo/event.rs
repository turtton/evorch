//! イベントログの追記と復元を管理します。

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use event_bus::{Event, EventKind};
use rusqlite::{Connection, Row, Transaction, TransactionBehavior, params};

use crate::db::{ns_to_system_time, system_time_to_ns};
use crate::entity::SecretGuard;
use crate::{HardLimits, LimitKind, StorageError};

mod accounting;
#[cfg(test)]
mod compaction_window_tests;
pub mod deferred;
pub(crate) mod diagnostic;
pub(crate) mod first_pass;
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
    /// Tombstones as of `data_version`; any other connection's commit
    /// (including a retention confirmation) invalidates it with the rest.
    tombstones: Option<Arc<crate::thread_retention::TombstoneSet>>,
}

/// 容量上限を検査してイベントを一件追記します。
pub fn append_event(
    conn: &Connection,
    session_id: Option<&str>,
    event: &Event,
    limits: &HardLimits,
    accounting: &mut EventAccounting,
) -> Result<StoredEvent, StorageError> {
    let transaction = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let mut next = accounting.clone();
    let stored = append_event_in_transaction(&transaction, session_id, event, limits, &mut next)?;
    transaction.commit()?;
    *accounting = next;
    Ok(stored)
}

/// The caller owns an IMMEDIATE transaction and commits accounting only after
/// that transaction commits. Used by handoff to commit its initial control state
/// atomically with responsibility, rather than depending on the async recorder.
pub(crate) fn append_event_in_transaction(
    transaction: &Transaction<'_>,
    session_id: Option<&str>,
    event: &Event,
    limits: &HardLimits,
    accounting: &mut EventAccounting,
) -> Result<StoredEvent, StorageError> {
    let conn: &Connection = transaction;
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

    // 他ウィンドウの writer がシードと INSERT の間に割り込まないよう、
    // 容量検査から commit まで同じ write transaction で保護します。
    let data_version: i64 = conn.pragma_query_value(None, "data_version", |row| row.get(0))?;
    // data_version は同じ接続自身の commit では変化しません。別接続の
    // commit を検出したときだけ再シードし、共有 DB の容量制限を維持します。
    let mut next_accounting = if accounting.data_version == Some(data_version) {
        accounting.clone()
    } else {
        EventAccounting::default()
    };
    next_accounting.data_version = Some(data_version);
    let tombstones = match &next_accounting.tombstones {
        Some(tombstones) => Arc::clone(tombstones),
        None => Arc::new(crate::thread_retention::TombstoneSet::load(conn)?),
    };
    // A deleted family accepts nothing new; refuse before any accounting moves.
    if let Some(error) = tombstones.rejects(&event.kind) {
        return Err(error);
    }
    next_accounting.tombstones = Some(tombstones);
    if next_accounting.seeded_session_id.as_deref() != session_id {
        next_accounting.session_bytes = match session_id {
            Some(id) => match next_accounting.sessions.iter().find(|(key, _)| key == id) {
                Some((_, bytes)) => *bytes,
                None => accounting::session_event_bytes(conn, id)?,
            },
            None => 0,
        };
        next_accounting.seeded_session_id = session_id.map(String::from);
    }
    let day_start = day_start_ns(event.meta.wall_clock)?;
    if next_accounting.seeded_day_start_ns != Some(day_start) {
        next_accounting.day_bytes = accounting::day_event_bytes(conn, day_start)?;
        next_accounting.seeded_day_start_ns = Some(day_start);
    }

    let monotonic_ns = i64::try_from(event.meta.monotonic.as_nanos())
        .map_err(|_| StorageError::OutOfRange("monotonic nanoseconds"))?;
    let wall_clock_ns = system_time_to_ns(event.meta.wall_clock)?;
    let schema_version = i64::from(event.meta.schema_version);
    let run_id = event.kind.primary_run_id();
    // The cost of the event: the stored payload plus the blobs this event created. The
    // limits are checked against it before the row is written, and an error rolls the
    // blobs back with everything else.
    let mut event_len = 0_u64;
    let mut write = |conn: &Connection,
                     encoded: &crate::codec::Encoded<'_>,
                     body_format: i64,
                     blob_bytes: u64|
     -> Result<i64, StorageError> {
        event_len = u64::try_from(encoded.stored_len())
            .map_err(|_| StorageError::OutOfRange("event payload length"))?
            .checked_add(blob_bytes)
            .ok_or(StorageError::OutOfRange("event payload length"))?;
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
        conn.execute(
            "INSERT INTO events \
             (session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload, subkind, \
              run_id, body_format, blob_bytes) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                session_id,
                schema_version,
                monotonic_ns,
                wall_clock_ns,
                kind,
                encoded,
                subkind,
                run_id,
                body_format,
                i64::try_from(blob_bytes)
                    .map_err(|_| StorageError::OutOfRange("event blob bytes"))?
            ],
        )?;
        Ok(conn.last_insert_rowid())
    };
    // A payload below the token threshold, without the escape character, has no string
    // that needs a token: JSON escaping never shortens a string.
    let mut tokenized = None;
    if compressible_payload(kind, subkind)
        && (payload.len() >= crate::body_ref::TOKEN_THRESHOLD_BYTES
            || payload.contains(crate::body_ref::ESCAPE))
    {
        let value = serde_json::from_str::<serde_json::Value>(&payload)
            .map_err(|error| StorageError::Serialization(error.to_string()))?;
        if crate::body_ref::any_needs_token(&value) {
            tokenized = Some([value]);
        }
    }
    let id = match tokenized.as_mut() {
        Some(containers) => {
            crate::body_store::put_owner_keyed(
                conn,
                &crate::body_store::EVENT_OWNER,
                containers,
                |conn, containers, created| {
                    let text = serde_json::to_string(&containers[0])
                        .map_err(|error| StorageError::Serialization(error.to_string()))?;
                    let encoded = encode_payload(kind, subkind, &text);
                    let id = write(conn, &encoded, 1, created.stored_bytes)?;
                    Ok((vec![rusqlite::types::Value::Integer(id)], id))
                },
            )?
            .0
        }
        None => {
            let encoded = encode_payload(kind, subkind, &payload);
            write(conn, &encoded, 0, 0)?
        }
    };

    let next_session_bytes = next_accounting.session_bytes + event_len;
    let next_day_bytes = next_accounting.day_bytes + event_len;
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

/// How [`rewrite_payload`] treats `events.blob_bytes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlobCharge {
    /// The row keeps the charge it has (an edit of a row that was stored already).
    Keep,
    /// The row is charged the blobs this call creates (the first tokenizing of a row).
    Created,
}

/// How [`rewrite_payload`] encodes the payload column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PayloadEncoding {
    /// As the writer stores a new event: a large text is a zstd frame.
    Writer,
    /// Always plain text; a later pass compresses it.
    Plain,
}

/// What [`rewrite_payload`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RewrittenPayload {
    pub format: StoredFormat,
    /// Bytes of the stored `payload` column.
    pub stored_len: u64,
    /// Blobs the rewrite created (physical figures, whatever `BlobCharge` says).
    pub created: crate::body_store::CreatedBlobs,
}

/// Replaces the payload of event `id` with `payload_json` (the event's JSON), as the
/// writer would store a new event of this `kind`/`subkind`: tokenized when it has a
/// string that needs a token and the kind is compressible, in one savepoint with the
/// blobs and the reference rows. Must run in a write transaction.
///
/// `payload_json` must be the plain form: a row that already holds tokens is resolved
/// first (a token string tokenized again would be stored as a body).
pub(crate) fn rewrite_payload(
    conn: &Connection,
    id: i64,
    kind: &str,
    subkind: &str,
    payload_json: &str,
    charge: BlobCharge,
    encoding: PayloadEncoding,
) -> Result<RewrittenPayload, StorageError> {
    fn encode<'a>(
        encoding: PayloadEncoding,
        kind: &str,
        subkind: &str,
        text: &'a str,
    ) -> crate::codec::Encoded<'a> {
        match encoding {
            PayloadEncoding::Writer => encode_payload(kind, subkind, text),
            PayloadEncoding::Plain => crate::codec::Encoded::Plain(text),
        }
    }
    let Some(value) = compressible_payload(kind, subkind)
        .then(|| {
            serde_json::from_str::<serde_json::Value>(payload_json)
                .map_err(|error| StorageError::Serialization(error.to_string()))
        })
        .transpose()?
    else {
        let encoded = encode(encoding, kind, subkind, payload_json);
        let stored_len = encoded.stored_len() as u64;
        conn.execute(
            "UPDATE events SET payload = ?2, body_format = 0 WHERE id = ?1",
            params![id, encoded],
        )?;
        return Ok(RewrittenPayload {
            format: StoredFormat::Inline,
            stored_len,
            created: crate::body_store::CreatedBlobs::default(),
        });
    };
    let mut containers = [value];
    let (written, created) = crate::body_store::put_owner_keyed(
        conn,
        &crate::body_store::EVENT_OWNER,
        &mut containers,
        |conn, containers, created| {
            let tokenized = containers[0].to_string();
            let format = if created_or_present_token(&containers[0]) {
                StoredFormat::Tokenized
            } else {
                StoredFormat::Inline
            };
            let encoded = encode(encoding, kind, subkind, &tokenized);
            let stored_len = encoded.stored_len() as u64;
            let charged = i64::try_from(created.stored_bytes)
                .map_err(|_| StorageError::OutOfRange("event blob bytes"))?;
            let format_column = i64::from(format == StoredFormat::Tokenized);
            match charge {
                BlobCharge::Keep => conn.execute(
                    "UPDATE events SET payload = ?2, body_format = ?3 WHERE id = ?1",
                    params![id, encoded, format_column],
                )?,
                BlobCharge::Created => conn.execute(
                    "UPDATE events SET payload = ?2, body_format = ?3, blob_bytes = ?4 WHERE id = ?1",
                    params![id, encoded, format_column, charged],
                )?,
            };
            Ok((
                vec![rusqlite::types::Value::Integer(id)],
                (format, stored_len),
            ))
        },
    )?;
    Ok(RewrittenPayload {
        format: written.0,
        stored_len: written.1,
        created,
    })
}

/// The bytes the writer would store for an event of this kind with this JSON, without
/// storing anything: the token container when a string needs a token, else the payload
/// itself, compressed as the writer does.
pub(crate) fn would_store_len(kind: &str, subkind: &str, payload_json: &str) -> usize {
    if compressible_payload(kind, subkind)
        && let Ok(mut value) = serde_json::from_str::<serde_json::Value>(payload_json)
        && crate::body_ref::any_needs_token(&value)
        && crate::body_ref::tokenize_in_place(&mut value, &mut |text| {
            Ok(crate::body_ref::BodyToken::of(text))
        })
        .is_ok()
    {
        return encode_payload(kind, subkind, &value.to_string()).stored_len();
    }
    encode_payload(kind, subkind, payload_json).stored_len()
}

/// Whether a tokenized container holds a token (it is the stored form of a row with
/// `body_format = 1`).
fn created_or_present_token(container: &serde_json::Value) -> bool {
    crate::body_ref::collect_tokens(container, "event").is_ok_and(|tokens| !tokens.is_empty())
}

/// セッションのイベントを採番順で返します。
pub fn list_by_session(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<StoredEvent>, StorageError> {
    list_events(
        conn,
        "SELECT id, session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload, body_format \
         FROM events WHERE session_id = ?1 ORDER BY id ASC",
        rusqlite::params![session_id],
    )
}

/// 全イベントを採番順で返します。
pub fn list_all_ordered(conn: &Connection) -> Result<Vec<StoredEvent>, StorageError> {
    list_events(
        conn,
        "SELECT id, session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload, body_format \
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
         CASE WHEN session_id = ?3 THEN NULL ELSE payload END, body_format \
         FROM events WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
    )?;
    let mut rows = statement.query(params![after_id, limit, exclude_session])?;
    let tombstones = crate::thread_retention::TombstoneSet::load(conn)?;
    let mut events = Vec::new();
    let mut last_id = after_id;
    let mut scanned = 0;
    while let Some(row) = rows.next()? {
        scanned += 1;
        last_id = row.get(0)?;
        if row.get_ref(6)? == rusqlite::types::ValueRef::Null {
            continue;
        }
        match row_to_event(conn, row) {
            Ok(event) if tombstones.rejects(&event.event.kind).is_some() => {}
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
        "SELECT id, session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload, body_format \
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
        "SELECT id, session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload, body_format \
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
    let tombstones = crate::thread_retention::TombstoneSet::load(conn)?;
    let mut skipped = 0_usize;
    while let Some(row) = rows.next()? {
        match row_to_event(conn, row) {
            Ok(event) if tombstones.rejects(&event.event.kind).is_some() => {}
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

/// Index of `body_format` in a row [`row_to_event`] reads.
pub(crate) const BODY_FORMAT_COLUMN: usize = 7;

/// The stored form of an event payload, as `events.body_format` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredFormat {
    /// The payload is the event's JSON.
    Inline,
    /// The payload is the event's JSON with the strings that needed a token replaced by
    /// their tokens (see `body_ref`).
    Tokenized,
}

impl StoredFormat {
    pub(crate) fn from_column(value: i64) -> Result<Self, StorageError> {
        match value {
            0 => Ok(Self::Inline),
            1 => Ok(Self::Tokenized),
            _ => Err(StorageError::OutOfRange("event body format")),
        }
    }
}

pub(super) fn row_meta(row: &Row<'_>) -> Result<event_bus::EventMeta, StorageError> {
    let schema_version: i64 = row.get(2)?;
    let monotonic_ns: i64 = row.get(3)?;
    let wall_clock_ns: i64 = row.get(4)?;
    Ok(event_bus::EventMeta {
        schema_version: u32::try_from(schema_version)
            .map_err(|_| StorageError::OutOfRange("event schema version"))?,
        monotonic: Duration::from_nanos(
            u64::try_from(monotonic_ns)
                .map_err(|_| StorageError::OutOfRange("monotonic nanoseconds"))?,
        ),
        wall_clock: ns_to_system_time(wall_clock_ns),
    })
}

/// The event's JSON of a stored payload (`value`) in the stored form `format`: the text
/// itself for an inline payload, the container with its tokens resolved for a tokenized
/// one. `label` names the owner in errors.
pub(crate) fn resolved_payload_json(
    conn: &Connection,
    value: rusqlite::types::ValueRef<'_>,
    format: StoredFormat,
    label: &str,
) -> Result<String, StorageError> {
    let payload = crate::codec::decode(value)?;
    match format {
        StoredFormat::Inline => Ok(payload.into_owned()),
        StoredFormat::Tokenized => {
            let mut container: serde_json::Value = serde_json::from_str(&payload)
                .map_err(|error| StorageError::Serialization(error.to_string()))?;
            crate::body_store::resolve_container(conn, label, &mut container)?;
            Ok(container.to_string())
        }
    }
}

/// Decodes a row of `id, session_id, schema_version, monotonic_ns, wall_clock_ns, kind,
/// payload` whose payload is the event's JSON as it stands: an event of a kind that is never
/// tokenized (Lifecycle, Orchestrator, ...). Maintenance that also runs on schemas older
/// than the token format reads those rows with it, because `body_format` may not exist.
pub(crate) fn row_to_plain_event(row: &Row<'_>) -> Result<StoredEvent, StorageError> {
    let meta = row_meta(row)?;
    let payload = crate::codec::decode(row.get_ref(6)?)?;
    let kind: EventKind = serde_json::from_str(&payload)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    Ok(StoredEvent {
        id: row.get(0)?,
        session_id: row.get(1)?,
        event: Event { meta, kind },
    })
}

/// Decodes a row of [`EVENT_COLUMNS`] into its event, resolving the tokens of a tokenized
/// payload through `conn` (read in the same transaction as the row, when it matters).
///
/// A payload that cannot be parsed or is not an event of this build is
/// `StorageError::Serialization`, which the readers skip row by row; a token that does not
/// resolve is `StorageError::BodyUnresolved`, which they never skip: a body that cannot
/// be read is not an event the caller may see without it.
pub(crate) fn row_to_event(conn: &Connection, row: &Row<'_>) -> Result<StoredEvent, StorageError> {
    let meta = row_meta(row)?;
    let format = StoredFormat::from_column(row.get(BODY_FORMAT_COLUMN)?)?;
    let payload = crate::codec::decode(row.get_ref(6)?)?;
    let kind: EventKind = match format {
        StoredFormat::Inline => serde_json::from_str(&payload)
            .map_err(|error| StorageError::Serialization(error.to_string()))?,
        StoredFormat::Tokenized => {
            let mut container: serde_json::Value = serde_json::from_str(&payload)
                .map_err(|error| StorageError::Serialization(error.to_string()))?;
            let id: i64 = row.get(0)?;
            crate::body_store::resolve_container(conn, &format!("event {id}"), &mut container)?;
            serde_json::from_value(container)
                .map_err(|error| StorageError::Serialization(error.to_string()))?
        }
    };
    Ok(StoredEvent {
        id: row.get(0)?,
        session_id: row.get(1)?,
        event: Event { meta, kind },
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

pub(crate) const fn kind_name(kind: &EventKind) -> &'static str {
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
        EventKind::PullRequest(_) => "PullRequest",
    }
}

#[cfg(test)]
mod body_tests;
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
