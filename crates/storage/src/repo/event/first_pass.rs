//! The first pass of the GUI history restore, without decoding the history.
//!
//! The restore needs three things from the whole event log before it replays
//! it: where each run last waited and whether its response had been completed
//! by then, the time of the last event the replay will accept, and the events
//! that rebuild the delegated-task rows. None of them needs a message or tool
//! body, so this pass reads the `kind`, `subkind` and `run_id` columns of every
//! row in ID order and looks into a payload only when it must:
//!
//! * the three lifecycle events that rebuild tasks are decoded in full, as the
//!   caller consumes them. A run's `Waiting` transition is one of them, so it
//!   needs no decode of its own;
//! * `MessagePart` rows are expanded for their metadata only (see below);
//! * `Started`/`Completed`/`Failed` lifecycle rows are read for their session ID,
//!   which the response reassembly treats as a run;
//! * rows whose `subkind` the backfill could not fill (`''` on a category that
//!   has an inner tag) cannot be classified, so they are decoded in full.
//!
//! **MessagePart exception.** Whether a response finished is decided by the
//! `response_id`/`index`/`total` continuity of its parts, which only the
//! payload holds (and the payload is usually compressed). A part is therefore
//! expanded to read those fields and the byte length of its `text`. The text is
//! never kept, concatenated or rebuilt: one expansion buffer exists at a time
//! and is dropped before the next row, and the only state carried between parts
//! is a counter of bytes per run, for at most [`MAX_PENDING_RUNS`] runs. The
//! expansion work is therefore bounded by the MessagePart payloads themselves.
//!
//! A database whose `subkind` column is not filled in yet (`subkind IS NULL`
//! somewhere) cannot be classified by column. The pass then decodes every row
//! exactly as the full replay does, and says so in [`FirstPassMetrics`], so the
//! caller can report the fallback once.
//!
//! One bad row never fails the pass: it is handed to `on_failure` and skipped.

use std::collections::BTreeMap;
use std::time::SystemTime;

use event_bus::{AgentRunPhase, Event, EventKind, LifecycleEvent, MessageEvent, ProviderEvent};
use rusqlite::{Connection, Row, types::ValueRef};
use serde::{Deserialize, Deserializer, de};

use super::{StoredEvent, StoredFormat, row_to_event, select};
use crate::StorageError;
use crate::db::ns_to_system_time;
use crate::thread_retention::TombstoneSet;

/// Unfinished responses kept at once, as in `event_bus::MessagePartAssembler`.
const MAX_PENDING_RUNS: usize = 64;
/// Text bytes of unfinished responses counted at once, as in the assembler.
const MAX_PENDING_BYTES: usize = 64 * 1024 * 1024;

/// What the first pass read, and how much of it it had to decode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FirstPassMetrics {
    /// Rows visited, whatever their kind.
    pub rows_scanned: u64,
    /// Events decoded only to learn a run's waiting state. The `Waiting`
    /// transition is read from the task decode, so this stays 0.
    pub waiting_event_decodes: u64,
    /// Lifecycle events decoded in full for the task restore.
    pub task_event_decodes: u64,
    /// `MessagePart` rows expanded for their metadata.
    pub message_part_metadata_rows: u64,
    /// Stored (possibly compressed) bytes of those rows' payloads.
    pub message_part_stored_bytes: u64,
    /// Bytes of those payloads after expansion.
    pub message_part_expanded_bytes: u64,
    /// `Started`/`Completed`/`Failed` rows read for their session ID.
    pub session_marker_rows: u64,
    /// Rows decoded in full because no column could classify them.
    pub unclassified_event_decodes: u64,
    /// Rows decoded to check their thread against the tombstones.
    pub thread_probe_decodes: u64,
    /// Rows that could not be read and were skipped.
    pub isolated_events: u64,
    /// The `subkind` column is not backfilled: every row was decoded.
    pub used_fallback: bool,
    /// Events decoded in full by the fallback.
    pub fallback_events_decoded: u64,
    /// Stored payload bytes read by the fallback.
    pub fallback_payload_bytes: u64,
}

/// The answer of the first pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FirstPassScan {
    /// ID of each run's last `AgentRunStateChanged` to `Waiting`.
    pub last_waiting: BTreeMap<String, i64>,
    /// Whether a response had completed since the run's last request when it
    /// last waited.
    pub completed_at_waiting: BTreeMap<String, bool>,
    /// The time of the last event the replay accepts: the last in ID order, a
    /// response counting at its final part.
    pub replay_end: Option<SystemTime>,
    /// The largest event ID of the snapshot the pass read. The replay that
    /// follows reads up to it and the live feed continues after it, so an event
    /// another process appends in between is neither skipped nor shown twice.
    pub through_id: i64,
    pub metrics: FirstPassMetrics,
}

/// Mirrors `event_bus::MessagePartAssembler`'s acceptance rules over lengths.
///
/// It keeps a byte count per unfinished run instead of the text, so it can
/// answer "did this part complete a response" without the response.
#[derive(Debug)]
struct PartTracker {
    open: BTreeMap<String, PendingResponse>,
    buffered_bytes: usize,
    next_order: u64,
    max_runs: usize,
    max_bytes: usize,
}

#[derive(Debug)]
struct PendingResponse {
    response_id: String,
    total: usize,
    next: usize,
    bytes: usize,
    order: u64,
}

impl PartTracker {
    const fn new() -> Self {
        Self::with_limits(MAX_PENDING_RUNS, MAX_PENDING_BYTES)
    }

    const fn with_limits(max_runs: usize, max_bytes: usize) -> Self {
        Self {
            open: BTreeMap::new(),
            buffered_bytes: 0,
            next_order: 0,
            max_runs,
            max_bytes,
        }
    }

    /// A run's non-part event closes its incomplete sequence.
    fn interrupt(&mut self, run: &str) {
        if let Some(pending) = self.open.remove(run) {
            self.buffered_bytes -= pending.bytes;
        }
    }

    /// Whether this part is the last one of a valid, complete response.
    fn accept(
        &mut self,
        run: &str,
        response_id: &str,
        index: usize,
        total: usize,
        text_len: usize,
    ) -> bool {
        if response_id.is_empty() || total == 0 || index >= total {
            self.interrupt(run);
            return false;
        }
        if index == 0 {
            self.interrupt(run);
            if self.max_runs == 0 {
                return false;
            }
            if self.open.len() >= self.max_runs {
                let Some(oldest) = self
                    .open
                    .iter()
                    .min_by_key(|(_, pending)| pending.order)
                    .map(|(run, _)| run.clone())
                else {
                    return false;
                };
                self.interrupt(&oldest);
            }
            self.open.insert(
                run.to_owned(),
                PendingResponse {
                    response_id: response_id.to_owned(),
                    total,
                    next: 0,
                    bytes: 0,
                    order: self.next_order,
                },
            );
            self.next_order = self.next_order.wrapping_add(1);
        }
        let Some(pending) = self.open.get_mut(run) else {
            return false;
        };
        if pending.response_id != response_id
            || pending.total != total
            || pending.next != index
            || self.buffered_bytes.saturating_add(text_len) > self.max_bytes
        {
            self.interrupt(run);
            return false;
        }
        pending.bytes += text_len;
        self.buffered_bytes += text_len;
        pending.next += 1;
        if pending.next != pending.total {
            return false;
        }
        self.interrupt(run);
        true
    }
}

/// What an accepted event does to the waiting bookkeeping.
#[derive(Debug, Clone, Copy)]
enum Effect<'a> {
    None,
    RequestStarted(&'a str),
    Completed(&'a str),
    Waiting(&'a str, i64),
}

struct Reducer {
    parts: PartTracker,
    last_waiting: BTreeMap<String, i64>,
    completed_at_waiting: BTreeMap<String, bool>,
    completed_since_request: BTreeMap<String, bool>,
    replay_end: Option<SystemTime>,
}

impl Reducer {
    const fn new(parts: PartTracker) -> Self {
        Self {
            parts,
            last_waiting: BTreeMap::new(),
            completed_at_waiting: BTreeMap::new(),
            completed_since_request: BTreeMap::new(),
            replay_end: None,
        }
    }

    /// An event the replay accepts as it is.
    fn accepted(&mut self, wall_clock: SystemTime, interrupt: Option<&str>, effect: Effect<'_>) {
        if let Some(run) = interrupt {
            self.parts.interrupt(run);
        }
        self.replay_end = Some(wall_clock);
        match effect {
            Effect::None => {}
            Effect::RequestStarted(run) => {
                self.completed_since_request.insert(run.to_owned(), false);
            }
            Effect::Completed(run) => {
                self.completed_since_request.insert(run.to_owned(), true);
            }
            Effect::Waiting(run, id) => {
                self.last_waiting.insert(run.to_owned(), id);
                self.completed_at_waiting.insert(
                    run.to_owned(),
                    self.completed_since_request
                        .get(run)
                        .copied()
                        .unwrap_or(false),
                );
            }
        }
    }

    /// One slice of a stored response; a final slice completes it.
    fn part(
        &mut self,
        wall_clock: SystemTime,
        run: &str,
        response_id: &str,
        index: usize,
        total: usize,
        text_len: usize,
    ) {
        if self.parts.accept(run, response_id, index, total, text_len) {
            self.accepted(wall_clock, None, Effect::Completed(run));
        }
    }

    /// An event decoded in full, as the fallback and the task restore see it.
    fn decoded(&mut self, stored: &StoredEvent) {
        let event = &stored.event;
        let wall_clock = event.meta.wall_clock;
        match &event.kind {
            EventKind::Message(MessageEvent::MessagePart {
                run_id,
                response_id,
                text,
                index,
                total,
            }) => self.part(wall_clock, run_id, response_id, *index, *total, text.len()),
            EventKind::Provider(ProviderEvent::RequestStarted {
                run_id: Some(run_id),
                ..
            }) => self.accepted(wall_clock, Some(run_id), Effect::RequestStarted(run_id)),
            EventKind::Message(MessageEvent::MessageCompleted { run_id, .. }) => {
                self.accepted(wall_clock, Some(run_id), Effect::Completed(run_id));
            }
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to: AgentRunPhase::Waiting,
                ..
            }) => self.accepted(wall_clock, Some(run_id), Effect::Waiting(run_id, stored.id)),
            _ => self.accepted(wall_clock, interrupted_run(event), Effect::None),
        }
    }
}

/// The run whose unfinished response a non-part event closes: its own run, or
/// the session of a session lifecycle event.
fn interrupted_run(event: &Event) -> Option<&str> {
    event.primary_run_id().or(match &event.kind {
        EventKind::Lifecycle(
            LifecycleEvent::Started { session_id, .. }
            | LifecycleEvent::Completed { session_id }
            | LifecycleEvent::Failed { session_id, .. },
        ) => Some(session_id.as_str()),
        _ => None,
    })
}

/// Whether the delegated-task restore consumes this event.
fn is_task_event(event: &Event) -> bool {
    matches!(
        &event.kind,
        EventKind::Lifecycle(
            LifecycleEvent::AgentRunStarted { .. }
                | LifecycleEvent::AgentRunStateChanged { .. }
                | LifecycleEvent::TurnCompleted { .. }
        )
    )
}

/// Rows the task restore consumes in full.
const TASK_SUBKINDS: &str = "'AgentRunStarted', 'AgentRunStateChanged', 'TurnCompleted'";
/// Lifecycle rows whose session ID closes an unfinished response.
const SESSION_SUBKINDS: &str = "'Started', 'Completed', 'Failed'";

/// The payload is selected only for rows the pass reads inside. SQLite
/// evaluates `CASE` lazily, so the other rows' payloads, including their
/// overflow pages, are never loaded. `?1` is whether some thread is tombstoned,
/// which makes thread goal and todo rows need a look at their thread.
fn columns_sql(read_payloads: bool) -> String {
    let payload = if read_payloads {
        "payload".to_owned()
    } else {
        format!(
            "CASE WHEN subkind IS NULL \
                OR (kind = 'Lifecycle' AND subkind IN ({TASK_SUBKINDS}, {SESSION_SUBKINDS})) \
                OR (kind = 'Message' AND subkind = 'MessagePart') \
                OR (subkind = '' AND kind NOT IN ('Diagnostic', 'Ownership')) \
                OR (?1 AND ((kind = 'Orchestrator' \
                    AND subkind IN ('ThreadGoalUpdated', 'ThreadTodoUpdated')) \
                    OR kind = 'PullRequest')) \
             THEN payload END"
        )
    };
    format!(
        "SELECT id, session_id, schema_version, monotonic_ns, wall_clock_ns, kind, \
         {payload}, body_format, subkind, run_id FROM events ORDER BY id ASC"
    )
}

const PAYLOAD: usize = 6;
const BODY_FORMAT: usize = super::BODY_FORMAT_COLUMN;
const SUBKIND: usize = 8;
const RUN_ID: usize = 9;

#[derive(Deserialize)]
struct Wrapped<T> {
    payload: T,
}

#[derive(Deserialize)]
struct SessionMarker {
    session_id: String,
}

/// The fields of a `MessagePart` that decide whether a response completed.
#[derive(Deserialize)]
struct PartMetadata {
    response_id: String,
    index: usize,
    total: usize,
    text: TextLen,
}

/// A JSON string read for its length alone: the decoded text is dropped as it
/// is visited. A string in a token position also keeps what it parses as, because in a
/// tokenized payload the length of the text is the token's, not the string's.
struct TextLen {
    raw: usize,
    token: Option<Result<crate::body_ref::BodyToken, crate::body_ref::TokenError>>,
}

impl TextLen {
    /// The byte length of the text the string stands for.
    fn text_len(&self, format: StoredFormat) -> Result<usize, StorageError> {
        match (format, &self.token) {
            (StoredFormat::Tokenized, Some(Ok(token))) => usize::try_from(token.byte_len)
                .map_err(|_| StorageError::OutOfRange("message part length")),
            (StoredFormat::Tokenized, Some(Err(error))) => Err(StorageError::BodyUnresolved {
                owner: "event".to_owned(),
                hash: None,
                reason: error.reason(),
            }),
            _ => Ok(self.raw),
        }
    }
}

impl<'de> Deserialize<'de> for TextLen {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = TextLen;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a string")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<TextLen, E> {
                Ok(TextLen {
                    raw: value.len(),
                    token: value
                        .starts_with(crate::body_ref::ESCAPE)
                        .then(|| crate::body_ref::BodyToken::parse(value)),
                })
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

fn column_str<'a>(row: &'a Row<'_>, index: usize) -> Option<&'a str> {
    match row.get_ref(index) {
        Ok(ValueRef::Text(bytes)) => std::str::from_utf8(bytes).ok(),
        _ => None,
    }
}

fn stored_len(value: ValueRef<'_>) -> u64 {
    match value {
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => bytes.len() as u64,
        _ => 0,
    }
}

/// How a row is read in the column-classified pass.
enum Reading {
    /// The columns say everything.
    Columns,
    /// Decoded in full: a task event, or a row nothing else can classify.
    Decode(Decoding),
    Part,
    SessionMarker,
}

#[derive(Clone, Copy)]
enum Decoding {
    Task,
    Unclassified,
    ThreadProbe,
}

fn classify(kind: &str, subkind: Option<&str>, probe_threads: bool) -> Reading {
    let Some(subkind) = subkind else {
        return Reading::Decode(Decoding::Unclassified);
    };
    match (kind, subkind) {
        ("Lifecycle", "AgentRunStarted" | "AgentRunStateChanged" | "TurnCompleted") => {
            Reading::Decode(Decoding::Task)
        }
        ("Lifecycle", "Started" | "Completed" | "Failed") => Reading::SessionMarker,
        ("Message", "MessagePart") => Reading::Part,
        ("Orchestrator", "ThreadGoalUpdated" | "ThreadTodoUpdated") | ("PullRequest", _)
            if probe_threads =>
        {
            Reading::Decode(Decoding::ThreadProbe)
        }
        // These categories have no inner tag, so a blank `subkind` is normal.
        ("Diagnostic" | "Ownership", _) => Reading::Columns,
        // The backfill found no variant of the category that fits the payload.
        (_, "") => Reading::Decode(Decoding::Unclassified),
        _ => Reading::Columns,
    }
}

pub(crate) fn scan(
    conn: &Connection,
    mut on_task_event: impl FnMut(&StoredEvent),
    mut on_failure: impl FnMut(i64, Option<&str>, &str),
) -> Result<FirstPassScan, StorageError> {
    // One read snapshot for the whole pass, so the fallback decision and every
    // row come from the same point in time.
    let transaction = conn.unchecked_transaction()?;
    let tombstones = TombstoneSet::load(&transaction)?;
    let fallback = select::has_unbackfilled(&transaction)?;
    let probe_threads = !tombstones.threads.is_empty();
    let through_id: i64 =
        transaction.query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |row| {
            row.get(0)
        })?;
    let mut metrics = FirstPassMetrics {
        used_fallback: fallback,
        ..FirstPassMetrics::default()
    };
    let mut reducer = Reducer::new(PartTracker::new());
    let mut statement = transaction.prepare(&columns_sql(fallback))?;
    let mut rows = if fallback {
        statement.query([])?
    } else {
        statement.query([probe_threads])?
    };
    while let Some(row) = rows.next()? {
        metrics.rows_scanned += 1;
        let id: i64 = row.get(0)?;
        let run_id = column_str(row, RUN_ID);
        let kind = column_str(row, 5);
        let reading = match kind {
            _ if fallback => Reading::Decode(Decoding::Unclassified),
            Some(kind) => classify(kind, column_str(row, SUBKIND), probe_threads),
            None => Reading::Decode(Decoding::Unclassified),
        };
        // A deleted run's rows stay on disk until reclaimed; the replay never
        // sees them, so they neither move the replay end nor close a response.
        let deleted = run_id.is_some_and(|run| tombstones.contains_run(run));
        if let Reading::Decode(decoding) = reading {
            if fallback {
                metrics.fallback_events_decoded += 1;
                metrics.fallback_payload_bytes += row.get_ref(PAYLOAD).map(stored_len).unwrap_or(0);
            } else {
                match decoding {
                    Decoding::Task => metrics.task_event_decodes += 1,
                    Decoding::Unclassified => metrics.unclassified_event_decodes += 1,
                    Decoding::ThreadProbe => metrics.thread_probe_decodes += 1,
                }
            }
            match row_to_event(&transaction, row) {
                // A row that cannot be read may still name a deleted run.
                Err(_) if deleted => {}
                Err(error) => isolate(&mut metrics, &mut on_failure, id, run_id, &error),
                Ok(stored) if tombstones.rejects(&stored.event.kind).is_some() => {}
                Ok(stored) => {
                    if is_task_event(&stored.event) {
                        on_task_event(&stored);
                    }
                    reducer.decoded(&stored);
                }
            }
            continue;
        }
        if deleted {
            continue;
        }
        // The decoder would refuse a row whose own columns are unusable, so a
        // pass that never decodes it must refuse it too.
        let wall_clock = match row_wall_clock(row) {
            Ok(wall_clock) => wall_clock,
            Err(error) => {
                isolate(&mut metrics, &mut on_failure, id, run_id, &error);
                continue;
            }
        };
        match reading {
            Reading::Columns => {
                let effect = match (kind, column_str(row, SUBKIND), run_id) {
                    (Some("Provider"), Some("RequestStarted"), Some(run)) => {
                        Effect::RequestStarted(run)
                    }
                    (Some("Message"), Some("MessageCompleted"), Some(run)) => {
                        Effect::Completed(run)
                    }
                    _ => Effect::None,
                };
                reducer.accepted(wall_clock, run_id, effect);
            }
            Reading::SessionMarker => {
                metrics.session_marker_rows += 1;
                match session_marker(row) {
                    Ok(session) => reducer.accepted(wall_clock, Some(&session), Effect::None),
                    Err(error) => isolate(&mut metrics, &mut on_failure, id, run_id, &error),
                }
            }
            Reading::Part => {
                metrics.message_part_metadata_rows += 1;
                match part_metadata(row, run_id, &mut metrics) {
                    Ok((run, meta, text_len)) => reducer.part(
                        wall_clock,
                        run,
                        &meta.response_id,
                        meta.index,
                        meta.total,
                        text_len,
                    ),
                    Err(error) => isolate(&mut metrics, &mut on_failure, id, run_id, &error),
                }
            }
            Reading::Decode(_) => {}
        }
    }
    Ok(FirstPassScan {
        last_waiting: reducer.last_waiting,
        completed_at_waiting: reducer.completed_at_waiting,
        replay_end: reducer.replay_end,
        through_id,
        metrics,
    })
}

fn isolate(
    metrics: &mut FirstPassMetrics,
    on_failure: &mut impl FnMut(i64, Option<&str>, &str),
    id: i64,
    run_id: Option<&str>,
    error: &StorageError,
) {
    metrics.isolated_events += 1;
    on_failure(id, run_id, &error.to_string());
}

/// The checks `row_to_event` makes of the columns around the payload, for rows
/// the pass reads without decoding them. Returns the event's wall-clock time.
fn row_wall_clock(row: &Row<'_>) -> Result<SystemTime, StorageError> {
    match row.get_ref(1)? {
        ValueRef::Null => {}
        ValueRef::Text(bytes) if std::str::from_utf8(bytes).is_ok() => {}
        _ => {
            return Err(StorageError::Serialization(
                "session_id column is not text".to_owned(),
            ));
        }
    }
    let schema_version: i64 = row.get(2)?;
    u32::try_from(schema_version).map_err(|_| StorageError::OutOfRange("event schema version"))?;
    let monotonic_ns: i64 = row.get(3)?;
    u64::try_from(monotonic_ns).map_err(|_| StorageError::OutOfRange("monotonic nanoseconds"))?;
    Ok(ns_to_system_time(row.get(4)?))
}

fn session_marker(row: &Row<'_>) -> Result<String, StorageError> {
    let payload = crate::codec::decode(row.get_ref(PAYLOAD)?)?;
    let marker: Wrapped<Wrapped<SessionMarker>> = serde_json::from_str(&payload)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    Ok(marker.payload.payload.session_id)
}

/// Expands one `MessagePart` row for its metadata, and nothing else.
fn part_metadata<'a>(
    row: &Row<'_>,
    run_id: Option<&'a str>,
    metrics: &mut FirstPassMetrics,
) -> Result<(&'a str, PartMetadata, usize), StorageError> {
    let format = StoredFormat::from_column(row.get(BODY_FORMAT)?)?;
    let value = row.get_ref(PAYLOAD)?;
    metrics.message_part_stored_bytes += stored_len(value);
    let payload = crate::codec::decode(value)?;
    metrics.message_part_expanded_bytes += payload.len() as u64;
    let run = run_id.ok_or_else(|| {
        StorageError::Serialization("MessagePart row has no run_id column".to_owned())
    })?;
    let meta: Wrapped<Wrapped<PartMetadata>> = serde_json::from_str(&payload)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    let meta = meta.payload.payload;
    let text_len = meta.text.text_len(format)?;
    Ok((run, meta, text_len))
}

/// Every event up to `through_id` in ID order, each reported as read or as failed. A row that
/// cannot be read is handed to `on_failure` with its ID and, when the `run_id`
/// column names one, its run, and the replay goes on. Only a failure of the
/// query itself ends it.
pub(crate) fn for_each_isolated(
    conn: &Connection,
    through_id: i64,
    mut visit: impl FnMut(StoredEvent),
    mut on_failure: impl FnMut(i64, Option<&str>, &str),
) -> Result<(), StorageError> {
    let tombstones = TombstoneSet::load(conn)?;
    let mut statement = conn.prepare(
        "SELECT id, session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload, \
         body_format, subkind, run_id FROM events WHERE id <= ?1 ORDER BY id ASC",
    )?;
    let mut rows = statement.query([through_id])?;
    while let Some(row) = rows.next()? {
        match row_to_event(conn, row) {
            Ok(stored) if tombstones.rejects(&stored.event.kind).is_some() => {}
            Ok(stored) => visit(stored),
            Err(error) => {
                let run_id = column_str(row, RUN_ID);
                if !run_id.is_some_and(|run| tombstones.contains_run(run)) {
                    on_failure(row.get(0)?, run_id, &error.to_string());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
