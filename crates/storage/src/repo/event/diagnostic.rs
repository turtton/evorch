//! Diagnostic-only selection and deletion on the writer connection.

use event_bus::EventKind;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::{DiagnosticCleanupScope, DiagnosticCleanupSummary, StorageError};

/// Cap examined rows, not only deletions: a long audit history cannot make one
/// automatic maintenance tick scan an unbounded number of protected events.
pub(crate) const SCAN_BATCH_SIZE: usize = 64;
pub(crate) type Cursor = (i64, i64);

struct Candidate {
    cursor: Cursor,
    session_id: Option<String>,
    payload: String,
}

fn candidates(
    conn: &Connection,
    scope: DiagnosticCleanupScope,
    after: Option<Cursor>,
) -> Result<Vec<Candidate>, StorageError> {
    // SQLite may seek a row-value comparison only by wall_clock_ns, then scan
    // every earlier ID sharing that timestamp. Use two explicit index ranges
    // so a large group of equally timestamped audits also has bounded cost.
    let mut rows = if after.is_some() {
        query_candidates(conn, scope, after, true, SCAN_BATCH_SIZE)?
    } else {
        Vec::new()
    };
    if rows.len() < SCAN_BATCH_SIZE {
        rows.extend(query_candidates(
            conn,
            scope,
            after,
            false,
            SCAN_BATCH_SIZE - rows.len(),
        )?);
    }
    Ok(rows)
}

fn query_candidates(
    conn: &Connection,
    scope: DiagnosticCleanupScope,
    after: Option<Cursor>,
    same_timestamp: bool,
    limit: usize,
) -> Result<Vec<Candidate>, StorageError> {
    let mut sql = String::from(
        "SELECT wall_clock_ns, id, session_id, payload FROM events \
         INDEXED BY idx_events_diagnostic_retention WHERE kind = 'Diagnostic'",
    );
    let mut values = Vec::new();
    if let DiagnosticCleanupScope::Before(cutoff) = scope {
        sql.push_str(" AND wall_clock_ns < ?");
        values.push(cutoff);
    }
    if let Some((time, id)) = after {
        if same_timestamp {
            sql.push_str(" AND wall_clock_ns = ? AND id > ?");
            values.extend([time, id]);
        } else {
            sql.push_str(" AND wall_clock_ns > ?");
            values.push(time);
        }
    }
    sql.push_str(" ORDER BY wall_clock_ns, id LIMIT ?");
    values.push(limit as i64);
    let mut statement = conn.prepare_cached(&sql)?;
    Ok(statement
        .query_map(rusqlite::params_from_iter(values), |row| {
            Ok(Candidate {
                cursor: (row.get(0)?, row.get(1)?),
                session_id: row.get(2)?,
                payload: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?)
}

fn disposable(candidate: &Candidate) -> bool {
    // Unknown or corrupt records are not safe to classify as disposable.
    matches!(serde_json::from_str::<EventKind>(&candidate.payload),
        Ok(EventKind::Diagnostic(event))
        if event.source != "sandbox" || event.code != "escalation_review")
}

fn process_batch(
    conn: &Connection,
    rows: &[Candidate],
    delete: bool,
    summary: &mut DiagnosticCleanupSummary,
) -> Result<(), StorageError> {
    for candidate in rows.iter().filter(|row| disposable(row)) {
        let bytes = candidate.payload.len() as u64;
        if delete {
            conn.execute("DELETE FROM events WHERE id = ?1", [candidate.cursor.1])?;
            // v12 DELETE triggers maintain authoritative quota aggregates. The
            // session projection also exposes a byte total and must agree.
            if let Some(session) = &candidate.session_id {
                conn.execute(
                    "UPDATE sessions SET total_event_bytes = MAX(0, total_event_bytes - ?1) \
                     WHERE id = ?2",
                    params![candidate.payload.len() as i64, session],
                )?;
            }
        }
        summary.event_count = summary.event_count.saturating_add(1);
        summary.payload_bytes = summary.payload_bytes.saturating_add(bytes);
    }
    Ok(())
}

/// Preview scans a consistent read-only snapshot on a separate connection.
pub(crate) fn preview(
    conn: &Connection,
    scope: DiagnosticCleanupScope,
) -> Result<DiagnosticCleanupSummary, StorageError> {
    let transaction = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;
    let mut summary = DiagnosticCleanupSummary::default();
    let mut cursor = None;
    loop {
        let rows = candidates(&transaction, scope, cursor)?;
        process_batch(&transaction, &rows, false, &mut summary)?;
        if rows.len() < SCAN_BATCH_SIZE {
            break;
        }
        cursor = rows.last().map(|row| row.cursor);
    }
    transaction.commit()?;
    Ok(summary)
}

/// Bound a manual sweep to the indexed range present when it started, so live
/// diagnostic appends beyond this cursor cannot prolong the operation forever.
pub(crate) fn last_cursor(
    conn: &Connection,
    scope: DiagnosticCleanupScope,
) -> Result<Option<Cursor>, StorageError> {
    let mut sql = String::from(
        "SELECT wall_clock_ns, id FROM events INDEXED BY idx_events_diagnostic_retention \
         WHERE kind = 'Diagnostic'",
    );
    let mut values = Vec::new();
    if let DiagnosticCleanupScope::Before(cutoff) = scope {
        sql.push_str(" AND wall_clock_ns < ?");
        values.push(cutoff);
    }
    sql.push_str(" ORDER BY wall_clock_ns DESC, id DESC LIMIT 1");
    Ok(conn
        .query_row(&sql, rusqlite::params_from_iter(values), |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?)
}

/// Commit one bounded manual batch. Returning an error guarantees this batch
/// removed nothing; earlier batches are reported separately by the caller.
pub(crate) fn cleanup_batch(
    conn: &Connection,
    scope: DiagnosticCleanupScope,
    after: Option<Cursor>,
    through: Cursor,
) -> Result<(DiagnosticCleanupSummary, Option<Cursor>), StorageError> {
    let transaction = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let mut rows = candidates(&transaction, scope, after)?;
    rows.retain(|row| row.cursor <= through);
    let mut summary = DiagnosticCleanupSummary::default();
    process_batch(&transaction, &rows, true, &mut summary)?;
    transaction.commit()?;
    let next = rows
        .last()
        .map(|row| row.cursor)
        .filter(|last| rows.len() == SCAN_BATCH_SIZE && *last < through);
    Ok((summary, next))
}

/// One bounded retention step. The cursor advances past protected diagnostics
/// too, and wraps after reaching the cutoff so backdated late arrivals will be
/// found on the following pass.
pub(crate) fn retention_batch(
    conn: &Connection,
    cutoff: i64,
    cursor: &mut Option<Cursor>,
) -> Result<DiagnosticCleanupSummary, StorageError> {
    let transaction = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let rows = candidates(
        &transaction,
        DiagnosticCleanupScope::Before(cutoff),
        *cursor,
    )?;
    let mut summary = DiagnosticCleanupSummary::default();
    process_batch(&transaction, &rows, true, &mut summary)?;
    transaction.commit()?;
    *cursor = if rows.len() == SCAN_BATCH_SIZE {
        rows.last().map(|row| row.cursor)
    } else {
        None
    };
    Ok(summary)
}
