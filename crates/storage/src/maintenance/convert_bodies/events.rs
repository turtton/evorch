//! The events step of `convert-bodies`: the payloads of Message, compaction and
//! compressible Tool events move to tokens and blobs, as a normal start now writes them.
//!
//! Rows are converted in `id` order, a bounded number per `IMMEDIATE` transaction and never
//! more than [`MAX_BATCH_PAYLOAD_BYTES`] of decoded payload together. Each event is
//! stored by the same function the writer uses (`rewrite_payload`), read back inside the
//! transaction through the reader's own resolution and compared, as a JSON value, with
//! what the row held before. Only `payload`, `body_format` and `blob_bytes` of a row
//! change; the verification compares everything else.
//!
//! The target of the step is decided from the row alone: `compressible_payload(kind,
//! subkind)` and at least one string that needs a token. A row whose JSON is parsed as a
//! generic value is converted even if this build can no longer read its variant, and no
//! field is dropped on the way.

use std::collections::{BTreeMap, HashSet};

use rusqlite::{Connection, Transaction, TransactionBehavior};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::StorageError;
use crate::body_ref::{BodyToken, any_needs_token, tokenize_in_place};
use crate::repo::event::{
    BlobCharge, PayloadEncoding, StoredFormat, compressible_payload, resolved_payload_json,
    rewrite_payload,
};

use super::super::invalid;
use super::runs::MISMATCH;
use super::source::decode_bounded;

/// Rows per transaction unless the options say otherwise.
pub const DEFAULT_BATCH_ROWS: usize = 1_000;
/// The most rows per transaction an option may ask for.
pub const MAX_BATCH_ROWS: usize = 100_000;
/// The most decoded payload bytes one transaction holds, whatever the row limit; also the
/// largest event the step converts.
pub const MAX_BATCH_PAYLOAD_BYTES: u64 = 16 * 1024 * 1024;
/// Problems a preview lists; the count goes on.
const MAX_LISTED_PROBLEMS: usize = 20;
/// Distinct bodies a preview remembers (40 bytes each).
const MAX_TRACKED_BODIES: usize = 2_000_000;

/// The kinds `compressible_payload` can accept; the scan reads no other row.
const KINDS_SQL: &str = "kind IN ('Tool', 'Message', 'compaction')";
/// A valid text payload below the token threshold without the escape character has no
/// string that needs a token (JSON escaping never shortens one), so only the other rows are
/// decoded. A short text that is not valid JSON is decoded too: it must be reported, never
/// skipped silently.
const MAY_NEED_TOKEN_SQL: &str = "(typeof(payload) = 'blob' OR OCTET_LENGTH(payload) >= 2048 \
     OR instr(payload, char(57344)) > 0 OR NOT json_valid(payload))";

/// The limit in force: [`MAX_BATCH_PAYLOAD_BYTES`], or a smaller one a test sets.
pub(super) fn payload_limit() -> u64 {
    #[cfg(test)]
    if let Some(limit) = limit_override::get() {
        return limit;
    }
    MAX_BATCH_PAYLOAD_BYTES
}

/// The plain bytes the scan expanded, counted where they are decoded (tests only).
#[cfg(test)]
pub(super) mod decoded {
    use std::cell::Cell;

    thread_local! {
        static BYTES: Cell<u64> = const { Cell::new(0) };
    }

    pub(super) fn add(bytes: u64) {
        BYTES.with(|cell| cell.set(cell.get() + bytes));
    }

    pub(in crate::maintenance::convert_bodies) fn take() -> u64 {
        BYTES.with(|cell| cell.replace(0))
    }
}

#[cfg(test)]
pub(super) mod limit_override {
    use std::cell::Cell;

    thread_local! {
        static LIMIT: Cell<Option<u64>> = const { Cell::new(None) };
    }

    pub(in crate::maintenance::convert_bodies) fn get() -> Option<u64> {
        LIMIT.with(Cell::get)
    }

    pub(in crate::maintenance::convert_bodies) fn set(limit: Option<u64>) {
        LIMIT.with(|cell| cell.set(limit));
    }
}

/// A seam for tests: called inside a batch's transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventPoint {
    /// After an event was stored, before it is read back.
    BeforeReadback { id: i64 },
    /// After the batch's last readback, before the commit.
    BeforeCommit,
    /// After the batch committed; the caller checkpoints the log here.
    AfterBatch { converted: u64 },
}

/// Events and bytes of one kind.
#[derive(Debug, Default, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct KindFigure {
    pub events: u64,
    /// Plain (decoded) payload bytes.
    pub plain_bytes: u64,
}

/// An event the step cannot convert, by id; never any of its text.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EventProblem {
    pub id: i64,
    pub reason: &'static str,
}

/// What the step found (preview) or did (`--apply`).
#[derive(Debug, Default, Serialize)]
pub struct EventsReport {
    /// Events that need a token and are not in token form yet.
    pub eligible_events: u64,
    pub eligible_plain_bytes: u64,
    /// The same by `kind/subkind`.
    pub eligible_by_kind: BTreeMap<String, KindFigure>,
    pub largest_event_plain_bytes: u64,
    /// Strings of the eligible events that become blobs, shared ones counted each time
    /// (preview).
    pub estimated_blob_candidates: u64,
    pub estimated_candidate_plain_bytes: u64,
    /// Distinct bodies seen; a lower bound once `distinct_tracking_capped` is set.
    pub estimated_distinct_blobs: u64,
    pub distinct_tracking_capped: bool,
    /// Events whose payload cannot be read, parsed, or whose columns disagree with it:
    /// `--apply` refuses while there is one.
    pub unconvertible_events: u64,
    /// Events above [`MAX_BATCH_PAYLOAD_BYTES`] of decoded payload: `--apply` refuses.
    pub oversized_events: u64,
    pub problems: Vec<EventProblem>,
    /// Upper bound of the growth of the database before the VACUUM (preview figure).
    pub estimated_growth_bytes: u64,
    pub converted_events: u64,
    /// Eligible events left after the step (0 once it succeeded).
    pub unconverted_remaining: u64,
    /// Events whose stored form differs from their old JSON value when read back.
    pub mismatched_events: u64,
    pub batches: u64,
    pub batch_rows_limit: usize,
    /// The most decoded payload bytes one batch held.
    pub max_batch_expanded_bytes: u64,
    /// The most log frames one batch's transaction left, read after its commit.
    pub peak_batch_wal_frames: u64,
    pub blobs_created: u64,
    pub blob_stored_bytes_created: u64,
    pub references_created: u64,
    /// Payload bytes before and after, and the accounting totals the persistent
    /// aggregates held (`payload + blob_bytes`; `blob_bytes` is a charge, not the
    /// physical size of the blobs).
    pub payload_stored_bytes_before: u64,
    pub payload_stored_bytes_after: u64,
    pub accounted_bytes_before: u64,
    pub accounted_bytes_after: u64,
}

/// Whether `events` already has the token-format columns.
pub(super) fn has_format_columns(conn: &Connection) -> Result<bool, StorageError> {
    let count: u64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('events') \
         WHERE name IN ('body_format', 'blob_bytes')",
        [],
        |row| row.get(0),
    )?;
    Ok(count == 2)
}

/// Whether `events` has either of the token-format columns (a half-prepared database has
/// only some).
pub(super) fn has_any_format_column(conn: &Connection) -> Result<bool, StorageError> {
    let count: u64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('events') \
         WHERE name IN ('body_format', 'blob_bytes')",
        [],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// `body_format` may be read only once the column exists.
fn unconverted_filter(conn: &Connection) -> Result<String, StorageError> {
    Ok(if has_format_columns(conn)? {
        format!("{KINDS_SQL} AND body_format = 0 AND {MAY_NEED_TOKEN_SQL}")
    } else {
        format!("{KINDS_SQL} AND {MAY_NEED_TOKEN_SQL}")
    })
}

/// The subkind of a payload read as a generic value: the inner tag of the externally
/// tagged event (`{"kind": ..., "payload": {"kind": <subkind>, ...}}`).
fn payload_subkind(value: &Value) -> Option<&str> {
    value.get("payload")?.get("kind")?.as_str()
}

/// What the scan decided about one row.
enum Verdict {
    /// Not a target: another kind, or no string that needs a token.
    Skip,
    Eligible {
        subkind: String,
        text: String,
        value: Value,
    },
    Problem(&'static str),
}

/// Decides one row from its columns and payload. `limit` bounds the decoded payload.
fn judge(
    kind: &str,
    column_subkind: Option<&str>,
    payload: rusqlite::types::ValueRef<'_>,
    limit: u64,
) -> Result<(Verdict, u64), StorageError> {
    let text = match decode_bounded(payload, limit) {
        Ok(text) => text,
        Err(reason) if reason.contains("larger than") => {
            return Ok((Verdict::Problem("OVERSIZED"), 0));
        }
        Err(_) => return Ok((Verdict::Problem("the payload cannot be read"), 0)),
    };
    let len = text.len() as u64;
    #[cfg(test)]
    decoded::add(len);
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Ok((Verdict::Problem("the payload is not JSON"), len));
    };
    let derived = payload_subkind(&value);
    let subkind = match (column_subkind.filter(|column| !column.is_empty()), derived) {
        (Some(column), Some(derived)) if column != derived => {
            return Ok((
                Verdict::Problem("the subkind column disagrees with the payload"),
                len,
            ));
        }
        (Some(column), _) => column.to_owned(),
        (None, Some(derived)) => derived.to_owned(),
        (None, None) => String::new(),
    };
    if !compressible_payload(kind, &subkind) || !any_needs_token(&value) {
        return Ok((Verdict::Skip, len));
    }
    Ok((
        Verdict::Eligible {
            subkind,
            text,
            value,
        },
        len,
    ))
}

/// Reads event `id` and decides it. Returns its kind, the verdict and the decoded size.
fn judge_row(
    conn: &Connection,
    id: i64,
    limit: u64,
) -> Result<(String, Verdict, u64), StorageError> {
    let mut statement =
        conn.prepare_cached("SELECT kind, subkind, payload FROM events WHERE id = ?1")?;
    let mut rows = statement.query([id])?;
    let Some(row) = rows.next()? else {
        return Err(invalid(format!(
            "event {id} disappeared during the conversion"
        )));
    };
    let kind: String = row.get(0)?;
    let subkind: Option<String> = row.get(1)?;
    let (verdict, len) = judge(&kind, subkind.as_deref(), row.get_ref(2)?, limit)?;
    Ok((kind, verdict, len))
}

/// The ids of up to `rows` events after `after` that may need a token.
fn candidate_ids(
    conn: &Connection,
    filter: &str,
    after: i64,
    rows: usize,
) -> Result<Vec<i64>, StorageError> {
    let limit = i64::try_from(rows).map_err(|_| StorageError::OutOfRange("event batch rows"))?;
    Ok(conn
        .prepare_cached(&format!(
            "SELECT id FROM events WHERE id > ?1 AND {filter} ORDER BY id LIMIT ?2"
        ))?
        .query_map(rusqlite::params![after, limit], |row| row.get(0))?
        .collect::<Result<_, _>>()?)
}

/// The read-only pass of `preview` and of `--apply`'s check before anything changes. It
/// decodes every candidate row once (one in memory at a time) and tokenizes it without
/// storing anything.
pub(super) fn analyze(conn: &Connection) -> Result<EventsReport, StorageError> {
    let filter = unconverted_filter(conn)?;
    let mut report = EventsReport::default();
    let mut bodies: HashSet<BodyToken> = HashSet::new();
    let limit = payload_limit();
    let mut after = i64::MIN;
    loop {
        let ids = candidate_ids(conn, &filter, after, DEFAULT_BATCH_ROWS)?;
        let Some(&last) = ids.last() else { break };
        for id in ids {
            let (kind, verdict, len) = judge_row(conn, id, limit)?;
            match verdict {
                Verdict::Skip => {}
                Verdict::Problem(reason) => {
                    if reason == "OVERSIZED" {
                        report.oversized_events += 1;
                    } else {
                        report.unconvertible_events += 1;
                    }
                    if report.problems.len() < MAX_LISTED_PROBLEMS {
                        report.problems.push(EventProblem {
                            id,
                            reason: if reason == "OVERSIZED" {
                                "a payload above the in-memory limit of one conversion batch"
                            } else {
                                reason
                            },
                        });
                    }
                }
                Verdict::Eligible {
                    subkind, mut value, ..
                } => {
                    report.eligible_events += 1;
                    report.eligible_plain_bytes += len;
                    report.largest_event_plain_bytes = report.largest_event_plain_bytes.max(len);
                    let figure = report
                        .eligible_by_kind
                        .entry(format!("{kind}/{subkind}"))
                        .or_default();
                    figure.events += 1;
                    figure.plain_bytes += len;
                    let mut capped = report.distinct_tracking_capped;
                    let mut candidates = 0_u64;
                    let mut candidate_bytes = 0_u64;
                    tokenize_in_place(&mut value, &mut |text| {
                        let token = BodyToken::of(text);
                        candidates += 1;
                        candidate_bytes += token.byte_len;
                        if bodies.len() < MAX_TRACKED_BODIES {
                            bodies.insert(token);
                        } else if !bodies.contains(&token) {
                            capped = true;
                        }
                        Ok(token)
                    })?;
                    report.distinct_tracking_capped = capped;
                    report.estimated_blob_candidates += candidates;
                    report.estimated_candidate_plain_bytes += candidate_bytes;
                }
            }
        }
        after = last;
    }
    report.estimated_distinct_blobs = bodies.len() as u64;
    // Blob candidates stored without compression with a quarter more for page waste, the
    // token containers (the original payloads in the worst case), and an allowance per
    // event, per blob or reference row and per index entry.
    let text = report
        .estimated_candidate_plain_bytes
        .saturating_add(report.eligible_plain_bytes);
    report.estimated_growth_bytes = text
        .saturating_add(text / 4)
        .saturating_add(report.eligible_events.saturating_mul(GROWTH_PER_EVENT))
        .saturating_add(
            report
                .estimated_blob_candidates
                .saturating_mul(GROWTH_PER_BLOB),
        )
        .saturating_add(GROWTH_FIXED);
    Ok(report)
}

/// Bytes an event row (its new payload column and the index entries around it), a blob and
/// a reference row add beyond the strings: deliberately generous.
const GROWTH_PER_EVENT: u64 = 256;
const GROWTH_PER_BLOB: u64 = 192;
/// The table, indexes and triggers the step creates, whatever the data.
const GROWTH_FIXED: u64 = 256 * 1024;

/// The persistent aggregates before the step.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Accounting {
    pub sessions: u64,
    pub days: u64,
    /// Sessions whose `total_event_bytes` equalled their aggregate: they must still, after
    /// the step. (A session whose total was already off is not the step's to explain.)
    pub consistent_sessions: Vec<String>,
}

pub(super) fn accounting(conn: &Connection) -> Result<Accounting, StorageError> {
    let scalar = |sql: &str| -> Result<u64, StorageError> {
        Ok(conn
            .query_row(sql, [], |row| row.get::<_, Option<u64>>(0))?
            .unwrap_or(0))
    };
    let consistent_sessions = conn
        .prepare(
            "SELECT s.id FROM sessions s WHERE s.total_event_bytes = COALESCE( \
             (SELECT payload_bytes FROM event_session_bytes WHERE session_id = s.id), 0)",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(Accounting {
        sessions: scalar("SELECT SUM(payload_bytes) FROM event_session_bytes")?,
        days: scalar("SELECT SUM(payload_bytes) FROM event_day_bytes")?,
        consistent_sessions,
    })
}

/// A digest of everything about the events that the step must not change.
pub(super) fn identity_fingerprint(conn: &Connection) -> Result<(u64, [u8; 32]), StorageError> {
    let mut statement = conn.prepare(
        "SELECT id, session_id, schema_version, monotonic_ns, wall_clock_ns, kind, subkind, \
         run_id FROM events ORDER BY id",
    )?;
    let mut rows = statement.query([])?;
    let mut digest = Sha256::new();
    let mut count = 0_u64;
    let field = |digest: &mut Sha256, tag: u8, bytes: &[u8]| {
        digest.update([tag]);
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    };
    while let Some(row) = rows.next()? {
        field(&mut digest, 0, &row.get::<_, i64>(0)?.to_le_bytes());
        for (index, tag) in [(1, 1_u8), (6, 6), (7, 7)] {
            match row.get::<_, Option<String>>(index)? {
                Some(text) => field(&mut digest, tag, text.as_bytes()),
                None => field(&mut digest, tag | 0x80, b""),
            }
        }
        for index in [2, 3, 4] {
            field(&mut digest, 2, &row.get::<_, i64>(index)?.to_le_bytes());
        }
        field(&mut digest, 5, row.get::<_, String>(5)?.as_bytes());
        count += 1;
    }
    Ok((count, digest.finalize().into()))
}

fn mismatch(id: i64, what: &str) -> StorageError {
    invalid(format!("{MISMATCH} in event {id}: {what}"))
}

/// What a finished batch reports to the caller.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct BatchOutcome {
    pub converted: u64,
    pub expanded_bytes: u64,
    pub last_id: Option<i64>,
}

/// Converts the next batch after `after` in one `IMMEDIATE` transaction. On any error
/// nothing of the batch is stored.
pub(super) fn convert_batch(
    conn: &Connection,
    filter: &str,
    after: i64,
    rows: usize,
    report: &mut EventsReport,
    hook: &mut dyn FnMut(EventPoint, &Connection) -> Result<(), StorageError>,
) -> Result<Option<BatchOutcome>, StorageError> {
    let ids = candidate_ids(conn, filter, after, rows)?;
    if ids.is_empty() {
        return Ok(None);
    }
    let limit = payload_limit();
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let mut outcome = BatchOutcome::default();
    let mut batch_report = BatchFigures::default();
    for id in ids {
        // The budget left is decided before the row is expanded: a row that does not fit
        // starts the next batch without having been decoded here.
        let remaining = limit.saturating_sub(outcome.expanded_bytes);
        let (kind, verdict, len) = judge_row(&tx, id, remaining)?;
        if outcome.expanded_bytes > 0 && matches!(verdict, Verdict::Problem("OVERSIZED")) {
            break;
        }
        outcome.expanded_bytes += len;
        outcome.last_id = Some(id);
        let (subkind, text, value) = match verdict {
            Verdict::Skip => continue,
            Verdict::Problem(reason) => {
                return Err(invalid(format!("event {id} cannot be converted: {reason}")));
            }
            Verdict::Eligible {
                subkind,
                text,
                value,
            } => (subkind, text, value),
        };
        let written = rewrite_payload(
            &tx,
            id,
            &kind,
            &subkind,
            &text,
            BlobCharge::Created,
            PayloadEncoding::Writer,
        )?;
        batch_report.blobs += written.created.count;
        batch_report.blob_bytes += written.created.stored_bytes;
        hook(EventPoint::BeforeReadback { id }, &tx)?;
        read_back(&tx, id, &value, written.format)?;
        outcome.converted += 1;
    }
    hook(EventPoint::BeforeCommit, &tx)?;
    tx.commit()?;
    report.blobs_created += batch_report.blobs;
    report.blob_stored_bytes_created += batch_report.blob_bytes;
    Ok(Some(outcome))
}

#[derive(Debug, Default)]
struct BatchFigures {
    blobs: u64,
    blob_bytes: u64,
}

/// Reads the event back the way a reader does and compares it with the old JSON value.
fn read_back(
    conn: &Connection,
    id: i64,
    old: &Value,
    format: StoredFormat,
) -> Result<(), StorageError> {
    let mut statement =
        conn.prepare_cached("SELECT body_format, payload FROM events WHERE id = ?1")?;
    let mut rows = statement.query([id])?;
    let Some(row) = rows.next()? else {
        return Err(mismatch(id, "the event is gone after the update"));
    };
    let stored_format = StoredFormat::from_column(row.get(0)?)?;
    if stored_format != format {
        return Err(mismatch(id, "the stored form is not the one written"));
    }
    let stored =
        resolved_payload_json(conn, row.get_ref(1)?, stored_format, &format!("event {id}"))
            .map_err(|error| mismatch(id, &format!("stored form unreadable: {error}")))?;
    let resolved: Value = serde_json::from_str(&stored)
        .map_err(|_| mismatch(id, "the stored payload is not JSON after the round trip"))?;
    if &resolved != old {
        return Err(mismatch(id, "the payload differs after the round trip"));
    }
    Ok(())
}

/// Checks that remain after the step: nothing left to convert, the aggregates equal what
/// the rows say, and nothing but the payload columns changed.
pub(super) fn verify(
    conn: &Connection,
    identity_before: (u64, [u8; 32]),
    accounting_before: Accounting,
    report: &mut EventsReport,
) -> Result<(), StorageError> {
    let remaining = analyze_remaining(conn)?;
    report.unconverted_remaining = remaining;
    if remaining != 0 {
        return Err(invalid(format!(
            "{remaining} events that need a token are still in the plain form"
        )));
    }
    if identity_fingerprint(conn)? != identity_before {
        return Err(invalid(
            "the identity, time, session, kind or attribution of an event changed during the conversion",
        ));
    }
    let after = accounting(conn)?;
    report.accounted_bytes_after = after.sessions;
    let expected_sessions = scalar_u64(
        conn,
        "SELECT SUM(bytes) FROM (SELECT SUM(OCTET_LENGTH(payload) + blob_bytes) AS bytes \
         FROM events WHERE session_id IS NOT NULL GROUP BY session_id)",
    )?;
    let expected_days = scalar_u64(
        conn,
        "SELECT SUM(OCTET_LENGTH(payload) + blob_bytes) FROM events",
    )?;
    if after.sessions != expected_sessions || after.days != expected_days {
        return Err(invalid(
            "the event accounting aggregates differ from the rows after the conversion",
        ));
    }
    for session in &accounting_before.consistent_sessions {
        let (total, aggregate): (u64, u64) = conn.query_row(
            "SELECT total_event_bytes, COALESCE( \
                 (SELECT payload_bytes FROM event_session_bytes WHERE session_id = ?1), 0) \
                 FROM sessions WHERE id = ?1",
            [session],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if total != aggregate {
            return Err(invalid(format!(
                "the total of session {session} did not follow the event accounting during the conversion"
            )));
        }
    }
    Ok(())
}

fn scalar_u64(conn: &Connection, sql: &str) -> Result<u64, StorageError> {
    Ok(conn
        .query_row(sql, [], |row| row.get::<_, Option<u64>>(0))?
        .unwrap_or(0))
}

/// How many events still need a token (a pass that stores nothing).
fn analyze_remaining(conn: &Connection) -> Result<u64, StorageError> {
    Ok(analyze(conn)?.eligible_events)
}

/// Runs the step: batches until no eligible event is left.
pub(super) fn convert_all(
    conn: &Connection,
    rows: usize,
    report: &mut EventsReport,
    hook: &mut dyn FnMut(EventPoint, &Connection) -> Result<(), StorageError>,
) -> Result<(), StorageError> {
    let filter = unconverted_filter(conn)?;
    report.batch_rows_limit = rows;
    let mut after = i64::MIN;
    while let Some(outcome) = convert_batch(conn, &filter, after, rows, report, hook)? {
        report.batches += 1;
        report.converted_events += outcome.converted;
        report.max_batch_expanded_bytes =
            report.max_batch_expanded_bytes.max(outcome.expanded_bytes);
        hook(
            EventPoint::AfterBatch {
                converted: outcome.converted,
            },
            conn,
        )?;
        match outcome.last_id {
            Some(last) => after = last,
            None => break,
        }
    }
    Ok(())
}
