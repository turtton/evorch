//! Event reads that find rows through the `kind`/`subkind` and `run_id`
//! indexes instead of scanning payloads.
//!
//! A read first collects the IDs of candidate rows from covering indexes, then
//! fetches the rows by ID in ascending order, so a long history is never held in
//! memory. Rows the backfill has not reached (`subkind IS NULL`) are not found
//! through the columns, so they are added as candidates that must be confirmed
//! by decoding the payload. Once the backfill is done the fallback costs one
//! probe of an empty index.

use std::collections::HashSet;
use std::ops::ControlFlow;

use event_bus::{AgentMessageEvent, EventKind, ToolEvent};
use rusqlite::{Connection, params};

use super::{StoredEvent, kind_name, row_to_event};
use crate::StorageError;

/// A row that may belong to a read's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) id: i64,
    /// Whether the row's payload must confirm the match: the row came from the
    /// unconverted fallback or from a supplement that no column covers.
    pub(crate) confirm: bool,
}

pub(crate) const UNCONVERTED_EXISTS_SQL: &str =
    "SELECT EXISTS (SELECT 1 FROM events WHERE subkind IS NULL)";
pub(crate) const IDS_BY_KIND_SQL: &str = "SELECT id FROM events WHERE kind = ?1";
pub(crate) const IDS_BY_SUBKIND_SQL: &str =
    "SELECT id FROM events WHERE kind = ?1 AND subkind = ?2";
pub(crate) const IDS_UNCONVERTED_BY_KIND_SQL: &str =
    "SELECT id FROM events WHERE kind = ?1 AND subkind IS NULL";
pub(crate) const IDS_BY_RUN_SQL: &str = "SELECT id FROM events WHERE run_id = ?1";
// `instr` cannot see inside a compressed payload, so every compressed row stays a
// candidate and the caller's confirmation decodes it.
pub(crate) const IDS_UNCONVERTED_MENTIONING_SQL: &str = "SELECT id FROM events WHERE subkind IS NULL \
     AND (typeof(payload) = 'blob' OR instr(payload, ?1) > 0)";
pub(crate) const IDS_BLANK_MENTIONING_SQL: &str = "SELECT id FROM events WHERE subkind = '' \
     AND (typeof(payload) = 'blob' OR instr(payload, ?1) > 0)";

/// Whether any row still lacks `subkind` and `run_id`.
pub(crate) fn has_unbackfilled(conn: &Connection) -> Result<bool, StorageError> {
    Ok(conn.query_row(UNCONVERTED_EXISTS_SQL, [], |row| row.get(0))?)
}

pub(crate) fn push_ids<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    confirm: bool,
    out: &mut Vec<Candidate>,
) -> Result<(), StorageError> {
    let mut statement = conn.prepare_cached(sql)?;
    let mut rows = statement.query(params)?;
    while let Some(row) = rows.next()? {
        out.push(Candidate {
            id: row.get(0)?,
            confirm,
        });
    }
    Ok(())
}

/// Sorts by ID and merges duplicates; a row needs no confirmation if any
/// source vouched for it.
pub(crate) fn normalize(mut candidates: Vec<Candidate>) -> Vec<Candidate> {
    candidates.sort_unstable_by_key(|candidate| candidate.id);
    let mut merged: Vec<Candidate> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        match merged.last_mut() {
            Some(last) if last.id == candidate.id => last.confirm &= candidate.confirm,
            _ => merged.push(candidate),
        }
    }
    merged
}

/// Hands the rows of `candidates` to `visit` in ID order. A row that needs
/// confirmation is dropped unless `accept` approves its decoded event; rows
/// older than `min_wall_clock_ns` and rows that no longer decode are skipped.
fn visit_candidates(
    conn: &Connection,
    candidates: &[Candidate],
    min_wall_clock_ns: i64,
    accept: impl Fn(&StoredEvent) -> bool,
    mut visit: impl FnMut(StoredEvent) -> ControlFlow<()>,
) -> Result<(), StorageError> {
    let mut statement = conn.prepare(
        "SELECT id, session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload \
         FROM events WHERE id = ?1",
    )?;
    let mut skipped = 0_usize;
    let tombstones = crate::thread_retention::TombstoneSet::load(conn)?;
    for candidate in candidates {
        let mut rows = statement.query(params![candidate.id])?;
        // The row may have been deleted since the IDs were collected.
        let Some(row) = rows.next()? else { continue };
        if row.get::<_, i64>(4)? < min_wall_clock_ns {
            continue;
        }
        match row_to_event(row) {
            // A deleted family's rows stay on disk until reclaimed; never show them.
            Ok(stored) if tombstones.rejects(&stored.event.kind).is_some() => {}
            Ok(stored) => {
                if (!candidate.confirm || accept(&stored)) && visit(stored).is_break() {
                    return Ok(());
                }
            }
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

/// Events of the given `(kind, subkind)` pairs in ID order. A `None` subkind
/// matches every subkind of the kind. `kind` is the stored category name.
pub(crate) fn for_each_matching(
    conn: &Connection,
    filters: &[(&str, Option<&str>)],
    mut visit: impl FnMut(StoredEvent),
) -> Result<(), StorageError> {
    let transaction = conn.unchecked_transaction()?;
    let unconverted = has_unbackfilled(&transaction)?;
    let mut candidates = Vec::new();
    for &(kind, subkind) in filters {
        match subkind {
            None => push_ids(
                &transaction,
                IDS_BY_KIND_SQL,
                params![kind],
                false,
                &mut candidates,
            )?,
            Some(subkind) => {
                push_ids(
                    &transaction,
                    IDS_BY_SUBKIND_SQL,
                    params![kind, subkind],
                    false,
                    &mut candidates,
                )?;
                if unconverted {
                    push_ids(
                        &transaction,
                        IDS_UNCONVERTED_BY_KIND_SQL,
                        params![kind],
                        true,
                        &mut candidates,
                    )?;
                }
            }
        }
    }
    let candidates = normalize(candidates);
    visit_candidates(
        &transaction,
        &candidates,
        i64::MIN,
        |stored| {
            let kind = kind_name(&stored.event.kind);
            filters.iter().any(|&(wanted, subkind)| {
                wanted == kind
                    && subkind.is_none_or(|subkind| subkind == stored.event.kind.subkind_name())
            })
        },
        |stored| {
            visit(stored);
            ControlFlow::Continue(())
        },
    )
}

/// The text of a run ID as it appears inside a JSON string, matched against the
/// payload to narrow rows that only the payload can attribute.
const RUN_MENTIONED: &str = "EXISTS (SELECT 1 FROM json_each(?1) AS mentioned \
     WHERE instr(payload, substr(json_quote(mentioned.value), 2, \
     length(json_quote(mentioned.value)) - 2)) > 0)";
/// [`RUN_MENTIONED`] for rows of unknown form (not backfilled): a compressed one
/// cannot be searched, so it stays a candidate. Kinds read through
/// `RUN_MENTIONED` alone are never compressed.
const RUN_MENTIONED_OR_COMPRESSED: &str = "(typeof(payload) = 'blob' OR EXISTS (SELECT 1 \
     FROM json_each(?1) AS mentioned WHERE instr(payload, substr(json_quote(mentioned.value), 2, \
     length(json_quote(mentioned.value)) - 2)) > 0))";

/// Tool subkinds attributed to a run by their call ID, which only the payload
/// holds. They are stored uncompressed so that `RUN_MENTIONED` can search them.
pub(crate) const PAYLOAD_ATTRIBUTED_SUBKINDS: [&str; 3] =
    ["ApprovalRequested", "ApprovalResolved", "ExecutionDenied"];

fn approval_subkinds_sql() -> String {
    let names: Vec<_> = PAYLOAD_ATTRIBUTED_SUBKINDS
        .iter()
        .map(|name| format!("'{name}'"))
        .collect();
    format!("({})", names.join(", "))
}

/// IDs of the events that concern `run_ids`: those whose `run_id` column names
/// one, plus the kinds the column cannot attribute (agent messages by sender,
/// approvals by call ID) and rows not converted yet.
pub(crate) fn run_candidates(
    conn: &Connection,
    run_ids: &[String],
) -> Result<Vec<Candidate>, StorageError> {
    let mut candidates = Vec::new();
    for run_id in run_ids {
        push_ids(
            conn,
            IDS_BY_RUN_SQL,
            params![run_id],
            false,
            &mut candidates,
        )?;
    }
    let listed = serde_json::to_string(run_ids)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    for sql in [
        format!("SELECT id FROM events WHERE kind = 'AgentMessage' AND {RUN_MENTIONED}"),
        format!(
            "SELECT id FROM events WHERE kind = 'Tool' AND subkind IN {} \
             AND {RUN_MENTIONED}",
            approval_subkinds_sql()
        ),
    ] {
        push_ids(conn, &sql, params![listed], true, &mut candidates)?;
    }
    if has_unbackfilled(conn)? {
        push_ids(
            conn,
            &format!(
                "SELECT id FROM events WHERE subkind IS NULL AND {RUN_MENTIONED_OR_COMPRESSED}"
            ),
            params![listed],
            true,
            &mut candidates,
        )?;
    }
    Ok(normalize(candidates))
}

/// Whether the event is attributed to one of `runs`: it is the run's own
/// event, an agent message sent to or from it, or an approval whose call ID
/// (`<run>:<call>`) belongs to it.
fn concerns_runs(event: &event_bus::Event, runs: &HashSet<&str>) -> bool {
    if event
        .primary_run_id()
        .is_some_and(|run_id| runs.contains(run_id))
    {
        return true;
    }
    match &event.kind {
        EventKind::AgentMessage(AgentMessageEvent::Delivered { message, .. }) => {
            runs.contains(message.sender_run_id.as_str())
                || runs.contains(message.recipient_run_id.as_str())
        }
        EventKind::Tool(
            ToolEvent::ApprovalRequested { call_id, .. }
            | ToolEvent::ApprovalResolved { call_id, .. }
            | ToolEvent::ExecutionDenied { call_id, .. },
        ) => call_id
            .split_once(':')
            .is_some_and(|(run, _)| runs.contains(run)),
        _ => false,
    }
}

/// Events concerning `run_ids` at or after `from_ns`, in ID order, until
/// `visit` breaks.
fn visit_runs(
    conn: &Connection,
    run_ids: &[String],
    from_ns: i64,
    visit: impl FnMut(StoredEvent) -> ControlFlow<()>,
) -> Result<(), StorageError> {
    let runs: HashSet<&str> = run_ids.iter().map(String::as_str).collect();
    let transaction = conn.unchecked_transaction()?;
    let candidates = run_candidates(&transaction, run_ids)?;
    visit_candidates(
        &transaction,
        &candidates,
        from_ns,
        |stored| concerns_runs(&stored.event, &runs),
        visit,
    )
}

/// Every event concerning `run_ids`, in ID order.
pub(crate) fn for_each_for_runs(
    conn: &Connection,
    run_ids: &[String],
    mut visit: impl FnMut(StoredEvent),
) -> Result<(), StorageError> {
    visit_runs(conn, run_ids, i64::MIN, |stored| {
        visit(stored);
        ControlFlow::Continue(())
    })
}

/// Up to `limit` events concerning `run_ids` at or after `from_ns`, oldest
/// first, and whether more matched.
pub(crate) fn list_for_runs(
    conn: &Connection,
    run_ids: &[String],
    from_ns: i64,
    limit: usize,
) -> Result<(Vec<StoredEvent>, bool), StorageError> {
    let mut events = Vec::new();
    let mut truncated = false;
    visit_runs(conn, run_ids, from_ns, |stored| {
        if events.len() == limit {
            truncated = true;
            return ControlFlow::Break(());
        }
        events.push(stored);
        ControlFlow::Continue(())
    })?;
    Ok((events, truncated))
}

/// Up to `limit` events concerning `run_ids` with `after_id < id <= through_id`,
/// oldest first, and whether more matched. The last ID of a page is the next
/// `after_id`, so a long history can be read page by page.
pub(crate) fn list_for_runs_between(
    conn: &Connection,
    run_ids: &[String],
    after_id: i64,
    through_id: i64,
    limit: usize,
) -> Result<(Vec<StoredEvent>, bool), StorageError> {
    let runs: HashSet<&str> = run_ids.iter().map(String::as_str).collect();
    let transaction = conn.unchecked_transaction()?;
    let candidates: Vec<Candidate> = run_candidates(&transaction, run_ids)?
        .into_iter()
        .filter(|candidate| candidate.id > after_id && candidate.id <= through_id)
        .collect();
    let mut events = Vec::new();
    let mut truncated = false;
    visit_candidates(
        &transaction,
        &candidates,
        i64::MIN,
        |stored| concerns_runs(&stored.event, &runs),
        |stored| {
            if events.len() == limit {
                truncated = true;
                return ControlFlow::Break(());
            }
            events.push(stored);
            ControlFlow::Continue(())
        },
    )?;
    Ok((events, truncated))
}
