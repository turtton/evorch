//! Persisted reindex requests (ADR 0041 R5).
//!
//! Registration freezes `(run, revision, versions)` targets and applies the
//! explicit-reindex R2 cell. It is bounded: the request row is created first as
//! `registering`, its targets are committed in batches (each a window of
//! [`FREEZE_BATCH`] runs in primary-key order, resumed by the last run id of the
//! previous window, so a batch reads at most one window and is its own short
//! transaction), the policy gaps of a disabled or paused index are closed in
//! windows of [`GAP_BATCH`] targets examined (a keyset over the request's own targets,
//! [`gap_batch`]), and one last short transaction turns the request into
//! `pending`. That transaction does work of a constant size: it neither re-checks nor
//! counts targets, because the steps before it already did. Each step is a separate
//! writer command, so the writer serves other commands in between.
//!
//! A `registering` request is not observable: status, settling, gap closing, the
//! wait deadlines and work selection only look at `pending` requests, and
//! [`abort_registering`] / [`cancel_stale_registering`] (the start-up sweep) make an
//! unfinished one `cancelled`. That is a matter of visibility only: the frozen
//! targets of a `registering` request are still rebound when a later registration
//! bumps the generation of their run (see [`rebind_shared`]), or they would
//! turn stale and close as gaps once the request is published.
//!
//! Targets are settled from the run rows (`sync`): success needs the same
//! revision/versions committed; every other terminal state is a reasoned gap. A
//! request terminalizes when no target is pending, after re-checking that
//! succeeded targets are still held. Terminal results never change afterwards.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, params};

use super::lease;
use crate::StorageError;
use crate::thread_index::{
    IndexBlockedReason, IndexTarget, ReindexRequest, ReindexResult, RequestFailure,
    RequestGapCount, RequestGapReason, RequestProgress, RequestStatus, RequestSummary,
};

/// `last_error_code` written next to `evicted_reason = 'oversized'`.
pub(crate) const OVERSIZED_STEP_CODE: &str = "oversized_step";
pub(crate) const OVERSIZED_BUDGET_CODE: &str = "oversized_budget";

pub(crate) fn validate(request: &ReindexRequest) -> Result<(), RequestFailure> {
    let bad_since = request.since_ns.is_some_and(|since| since < 0);
    let bad_project = request
        .canonical_project
        .as_deref()
        .is_some_and(|project| project.trim().is_empty());
    if bad_since || bad_project {
        return Err(RequestFailure::InvalidArguments);
    }
    Ok(())
}

fn to_json(value: &impl serde::Serialize) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|error| StorageError::Serialization(error.to_string()))
}

fn from_json<T: serde::de::DeserializeOwned>(json: &str) -> Result<T, StorageError> {
    serde_json::from_str(json).map_err(|error| StorageError::Serialization(error.to_string()))
}

fn insert_request(
    conn: &Connection,
    request: &ReindexRequest,
    now_ns: i64,
) -> Result<String, StorageError> {
    for attempt in 0_u32.. {
        let request_id = format!("reindex-{now_ns:016x}-{attempt}");
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO thread_index_requests(request_id, canonical_project, since_ns, \
             include_evicted, state, created_at_ns) VALUES (?1, ?2, ?3, ?4, 'registering', ?5)",
            params![
                request_id,
                request.canonical_project,
                request.since_ns,
                request.include_evicted,
                now_ns
            ],
        )?;
        if inserted == 1 {
            return Ok(request_id);
        }
    }
    unreachable!("u32 request id suffixes exhausted")
}

/// Rebinds every live target sharing `(run, revision, versions)` to the run's
/// current generation, so shared work stays valid for all of them.
///
/// "Live" includes a request that is still `registering`: its already frozen targets
/// must follow a generation bump made by a later registration, or they would turn
/// stale and close as gaps once the request is published. A `registering` request
/// ends only as `pending` (published) or `cancelled` (aborted / swept by the next registration),
/// never as a terminal result, so it is not a source of rebinding itself.
pub(crate) fn rebind_shared(conn: &Connection, run_id: &str) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE thread_index_request_targets AS t SET work_generation = r.work_generation \
         FROM thread_index_runs AS r \
         WHERE r.run_id = t.run_id AND t.run_id = ?1 AND t.state = 'pending' \
         AND t.source_revision = r.source_revision \
         AND t.extractor_version = r.extractor_version \
         AND t.accounting_version = r.accounting_version \
         AND t.work_generation <> r.work_generation \
         AND t.request_id IN (SELECT request_id FROM thread_index_requests \
              WHERE state IN ('pending', 'registering'))",
        [run_id],
    )?;
    Ok(())
}

/// Runs one registration step as its own short transaction.
fn immediate<T>(
    conn: &Connection,
    body: impl FnOnce(&Connection) -> Result<T, StorageError>,
) -> Result<T, StorageError> {
    let transaction =
        rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let value = body(&transaction)?;
    transaction.commit()?;
    Ok(value)
}

/// Runs of one registration batch (and of one discovery batch).
pub(crate) const FREEZE_BATCH: usize = 256;

/// Where the next freeze batch resumes: the last run examined by the previous one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FreezeCursor {
    pub run_id: String,
}

/// Targets one policy-gap batch closes at most.
pub(crate) const GAP_BATCH: usize = 256;

/// One committed freeze batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FrozenBatch {
    /// `None` when the scan reached the end.
    pub next: Option<FreezeCursor>,
    pub targets: u64,
}

/// Creates the `registering` request row (the first, tiny step of a registration).
pub(crate) fn begin_registration(
    conn: &Connection,
    request: &ReindexRequest,
    now_ns: i64,
) -> Result<String, StorageError> {
    immediate(conn, |conn| insert_request(conn, request, now_ns))
}

fn registering_request(
    conn: &Connection,
    request_id: &str,
) -> Result<ReindexRequest, StorageError> {
    conn.query_row(
        "SELECT canonical_project, since_ns, include_evicted FROM thread_index_requests \
         WHERE request_id = ?1 AND state = 'registering'",
        [request_id],
        |row| {
            Ok(ReindexRequest {
                canonical_project: row.get(0)?,
                since_ns: row.get(1)?,
                include_evicted: row.get(2)?,
            })
        },
    )
    .optional()?
    .ok_or(StorageError::StaleMutation)
}

/// Freezes the next batch of targets in one short transaction. A failed batch
/// commits nothing, so retrying it with the same cursor is exact; a batch that
/// is repeated after it committed cannot add a target twice (the cursor moved).
pub(crate) fn freeze_batch(
    conn: &Connection,
    request_id: &str,
    cursor: Option<&FreezeCursor>,
    limit: usize,
) -> Result<FrozenBatch, StorageError> {
    immediate(conn, |conn| {
        let request = registering_request(conn, request_id)?;
        // The window is the next `limit` runs after the cursor in primary-key order (an
        // index seek, never a scan of the table); the filters are applied to those rows
        // only. A run of a thread that is archived now is never a target, whatever its
        // state or why it was evicted: only unarchiving brings it back.
        let archived = crate::repo::thread_index::archived_run_predicate("w.run_id");
        let after = if cursor.is_some() {
            "WHERE run_id < ?4"
        } else {
            ""
        };
        let mut statement = conn.prepare(&format!(
            "SELECT w.run_id, w.state, (NOT {archived} \
             AND (?1 IS NULL OR w.source_updated_at_ns >= ?1) \
             AND (?2 IS NULL OR EXISTS (SELECT 1 FROM thread_runs tr \
                  JOIN threads t ON t.thread_id = tr.thread_id \
                  WHERE tr.run_id = w.run_id AND t.project_root = ?2))) \
             FROM (SELECT run_id, state, source_updated_at_ns FROM thread_index_runs {after} \
                   ORDER BY run_id DESC LIMIT ?3) w"
        ))?;
        let window = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut values: Vec<&dyn rusqlite::ToSql> =
            vec![&request.since_ns, &request.canonical_project, &window];
        if let Some(cursor) = cursor {
            values.push(&cursor.run_id);
        }
        let mut examined = statement
            .query_map(values.as_slice(), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        examined.sort_by(|a, b| b.0.cmp(&a.0));
        let complete = examined.len() < limit;
        let next = examined
            .last()
            .filter(|_| !complete)
            .map(|(run_id, _, _)| FreezeCursor {
                run_id: run_id.clone(),
            });
        let mut deleted = 0_u64;
        let mut targets = 0_u64;
        for (run_id, state, selected) in examined {
            if !selected {
                continue;
            }
            match state.as_str() {
                "deleted" => {
                    deleted += 1;
                    continue;
                }
                "evicted" if !request.include_evicted => continue,
                _ => {}
            }
            // R2 "明示 reindex": back to pending, attempts reset, new generation.
            conn.execute(
                "UPDATE thread_index_runs SET state = 'pending', attempt_count = 0, \
                 next_attempt_at_ns = NULL, blocked_reason = NULL, evicted_reason = NULL, \
                 last_error_code = NULL, work_generation = work_generation + 1 WHERE run_id = ?1",
                [&run_id],
            )?;
            rebind_shared(conn, &run_id)?;
            conn.execute(
                "INSERT INTO thread_index_request_targets(request_id, run_id, source_revision, \
                 extractor_version, accounting_version, work_generation, state) \
                 SELECT ?1, run_id, source_revision, extractor_version, accounting_version, \
                 work_generation, 'pending' FROM thread_index_runs WHERE run_id = ?2",
                params![request_id, run_id],
            )?;
            targets += 1;
        }
        if deleted > 0 {
            conn.execute(
                "UPDATE thread_index_requests SET \
                 deleted_before_submission = deleted_before_submission + ?2 WHERE request_id = ?1",
                params![request_id, deleted],
            )?;
        }
        Ok(FrozenBatch { next, targets })
    })
}

/// Last step, inside the caller's short transaction: the frozen request becomes
/// `pending` (or `completed` when it froze nothing).
pub(crate) fn publish_registration(
    conn: &Connection,
    request_id: &str,
    now_ns: i64,
) -> Result<(), StorageError> {
    let published = conn.execute(
        "UPDATE thread_index_requests SET state = 'pending' \
         WHERE request_id = ?1 AND state = 'registering'",
        [request_id],
    )?;
    if published != 1 {
        return Err(StorageError::StaleMutation);
    }
    conn.execute(
        "UPDATE thread_index_requests SET state = 'completed', finished_at_ns = ?2 \
         WHERE request_id = ?1 AND NOT EXISTS (SELECT 1 FROM thread_index_request_targets \
         WHERE request_id = ?1)",
        params![request_id, now_ns],
    )?;
    Ok(())
}

/// An interrupted registration of this invocation ends `cancelled`.
pub(crate) fn abort_registering(
    conn: &Connection,
    request_id: &str,
    now_ns: i64,
) -> Result<bool, StorageError> {
    Ok(conn.execute(
        "UPDATE thread_index_requests SET state = 'cancelled', finished_at_ns = ?2 \
         WHERE request_id = ?1 AND state = 'registering'",
        params![request_id, now_ns],
    )? == 1)
}

/// Start-up sweep: registrations left behind by an interrupted process end `cancelled`
/// (the index itself was never touched). `live` names the registrations a writer of
/// this process is still running; those are never swept. Returns how many were closed.
pub(crate) fn cancel_stale_registering(
    conn: &Connection,
    live: &[String],
    now_ns: i64,
) -> Result<usize, StorageError> {
    // One statement; `live` is the set of this process's own registrations (a handful).
    let live = to_json(&live)?;
    Ok(conn.execute(
        "UPDATE thread_index_requests SET state = 'cancelled', finished_at_ns = ?1 \
         WHERE state = 'registering' \
         AND request_id NOT IN (SELECT value FROM json_each(?2))",
        params![now_ns, live],
    )?)
}

/// Where the next policy-gap batch resumes: the last target examined by the previous one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GapCursor {
    pub run_id: String,
}

/// What one policy-gap batch did.
#[derive(Debug)]
pub(crate) struct GapBatch {
    /// `None` when the scan reached the end of the registration's targets.
    pub next: Option<GapCursor>,
}

/// The update of a gap window that has a last run: a range of the primary key closed on
/// both sides (a test pins its query plan).
pub(crate) const GAP_CLOSED_WINDOW_SQL: &str = "UPDATE thread_index_request_targets \
     SET state = 'gap', gap_reason = ?2 \
     WHERE request_id = ?1 AND run_id > ?3 AND run_id <= ?4 AND state = 'pending'";

/// Closes the pending targets of the registration `request_id`, and only those, with
/// `reason`: the targets of a disabled or paused index cannot be worked on. One short
/// transaction per call over a window of at most `limit` targets after `cursor` in
/// run-id order (a primary-key seek; the budget counts the targets examined, whether or
/// not they were pending). A caller repeats it with the returned cursor until it is
/// `None`.
///
/// The rows of any other request are not touched: a published request ends terminal
/// through its owner, which is the only terminalizer of a published request (its step, or
/// the transaction that stops it).
pub(crate) fn gap_batch(
    conn: &Connection,
    request_id: &str,
    reason: RequestGapReason,
    cursor: Option<&GapCursor>,
    limit: usize,
) -> Result<GapBatch, StorageError> {
    immediate(conn, |conn| {
        let reason = to_json(&reason)?;
        let after = cursor.map_or("", |cursor| cursor.run_id.as_str());
        // The last run of the window: the `limit`-th target after the cursor, when there
        // is one (an offset of at most `limit` rows).
        let last: Option<String> = conn
            .query_row(
                "SELECT run_id FROM thread_index_request_targets \
                 WHERE request_id = ?1 AND run_id > ?2 ORDER BY run_id LIMIT 1 OFFSET ?3",
                params![
                    request_id,
                    after,
                    i64::try_from(limit.saturating_sub(1)).unwrap_or(i64::MAX)
                ],
                |row| row.get(0),
            )
            .optional()?;
        // Two statements, not one with `(?4 IS NULL OR run_id <= ?4)`: that OR keeps the
        // upper bound out of the index range, so a window would read every remaining
        // target. Each form is a range of the primary key closed by the window (the
        // last window is open-ended, and holds fewer than `limit` targets).
        match &last {
            Some(last) => conn.execute(
                GAP_CLOSED_WINDOW_SQL,
                params![request_id, reason, after, last],
            )?,
            None => conn.execute(
                "UPDATE thread_index_request_targets SET state = 'gap', gap_reason = ?2 \
                 WHERE request_id = ?1 AND run_id > ?3 AND state = 'pending'",
                params![request_id, reason, after],
            )?,
        };
        Ok(GapBatch {
            next: last.map(|run_id| GapCursor { run_id }),
        })
    })
}

/// Inside the publish transaction of a registration whose targets the gap batches all
/// closed: the request ends `completed_with_gaps` right away. Constant work (a primary
/// key update); a request that froze nothing was already `completed` by the publish.
pub(crate) fn complete_closed_registration(
    conn: &Connection,
    request_id: &str,
    now_ns: i64,
) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE thread_index_requests SET state = 'completed_with_gaps', finished_at_ns = ?2 \
         WHERE request_id = ?1 AND state = 'pending'",
        params![request_id, now_ns],
    )?;
    Ok(())
}

/// Run columns a target is judged against.
struct RunView {
    state: String,
    target: IndexTarget,
    work_generation: u64,
    indexed: Option<(u64, u32, u32)>,
    evicted_reason: Option<String>,
    blocked_reason: Option<String>,
    last_error_code: Option<String>,
}

enum Outcome {
    Keep,
    Rebind(u64),
    Succeeded,
    Gap(RequestGapReason),
}

fn gap_for(run: &RunView) -> Result<RequestGapReason, StorageError> {
    Ok(match (run.state.as_str(), run.evicted_reason.as_deref()) {
        ("evicted", Some("oversized")) => {
            if run.last_error_code.as_deref() == Some(OVERSIZED_STEP_CODE) {
                RequestGapReason::OversizedStep
            } else {
                RequestGapReason::OversizedBudget
            }
        }
        ("evicted", Some("physical_reclaim")) => RequestGapReason::CapacityPaused,
        ("evicted", _) => RequestGapReason::EvictedByBudget,
        ("deleted", _) => RequestGapReason::Deleted,
        ("blocked", _) => RequestGapReason::Blocked(
            run.blocked_reason
                .as_deref()
                .map(from_json::<IndexBlockedReason>)
                .transpose()?
                .unwrap_or(IndexBlockedReason::AttemptsExhausted),
        ),
        // A held revision was replaced by a pending re-extraction elsewhere.
        _ => RequestGapReason::EvictedByBudget,
    })
}

fn judge(
    target: &IndexTarget,
    generation: u64,
    run: Option<&RunView>,
    terminal: bool,
) -> Result<Outcome, StorageError> {
    let Some(run) = run else {
        return Ok(Outcome::Gap(RequestGapReason::Deleted));
    };
    if run.state == "deleted" {
        return Ok(Outcome::Gap(RequestGapReason::Deleted));
    }
    let held = run.indexed
        == Some((
            target.source_revision,
            target.extractor_version,
            target.accounting_version,
        ));
    if held && (terminal || run.state == "indexed") {
        return Ok(Outcome::Succeeded);
    }
    if run.target.source_revision != target.source_revision {
        return Ok(Outcome::Gap(RequestGapReason::Superseded));
    }
    if run.target.extractor_version != target.extractor_version
        || run.target.accounting_version != target.accounting_version
    {
        return Ok(Outcome::Gap(RequestGapReason::VersionSuperseded));
    }
    if terminal {
        return gap_for(run).map(Outcome::Gap);
    }
    match run.state.as_str() {
        "pending" | "retry_wait" if run.work_generation != generation => {
            Ok(Outcome::Rebind(run.work_generation))
        }
        "pending" | "retry_wait" => Ok(Outcome::Keep),
        _ => gap_for(run).map(Outcome::Gap),
    }
}

fn run_view(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<Option<RunView>> {
    let Some(state) = row.get::<_, Option<String>>(offset)? else {
        return Ok(None);
    };
    let indexed_revision: Option<u64> = row.get(offset + 5)?;
    let indexed_extractor: Option<u32> = row.get(offset + 6)?;
    let indexed_accounting: Option<u32> = row.get(offset + 7)?;
    Ok(Some(RunView {
        state,
        target: IndexTarget {
            run_id: String::new(),
            source_revision: row.get(offset + 1)?,
            extractor_version: row.get(offset + 2)?,
            accounting_version: row.get(offset + 3)?,
        },
        work_generation: row.get(offset + 4)?,
        indexed: indexed_revision
            .zip(indexed_extractor)
            .zip(indexed_accounting)
            .map(|((revision, extractor), accounting)| (revision, extractor, accounting)),
        evicted_reason: row.get(offset + 8)?,
        blocked_reason: row.get(offset + 9)?,
        last_error_code: row.get(offset + 10)?,
    }))
}

const RUN_COLUMNS: &str = "r.state, r.source_revision, r.extractor_version, r.accounting_version, \
     r.work_generation, r.indexed_revision, r.indexed_extractor_version, \
     r.indexed_accounting_version, r.evicted_reason, r.blocked_reason, r.last_error_code";

type Judged = (String, String, IndexTarget, u64, Option<RunView>);

fn targets_with_runs(
    conn: &Connection,
    target_state: &str,
    only_request: Option<&str>,
) -> Result<Vec<Judged>, StorageError> {
    let mut statement = conn.prepare(&format!(
        "SELECT t.request_id, t.run_id, t.source_revision, t.extractor_version, \
         t.accounting_version, t.work_generation, {RUN_COLUMNS} \
         FROM thread_index_request_targets t \
         JOIN thread_index_requests q ON q.request_id = t.request_id AND q.state = 'pending' \
         LEFT JOIN thread_index_runs r ON r.run_id = t.run_id \
         WHERE t.state = ?1 AND (?2 IS NULL OR t.request_id = ?2)"
    ))?;
    let rows = statement
        .query_map(params![target_state, only_request], |row| {
            let run_id: String = row.get(1)?;
            Ok((
                row.get(0)?,
                run_id.clone(),
                IndexTarget {
                    run_id,
                    source_revision: row.get(2)?,
                    extractor_version: row.get(3)?,
                    accounting_version: row.get(4)?,
                },
                row.get(5)?,
                run_view(row, 6)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn set_target(
    conn: &Connection,
    request_id: &str,
    run_id: &str,
    outcome: &Outcome,
) -> Result<(), StorageError> {
    match outcome {
        Outcome::Keep => {}
        Outcome::Rebind(generation) => {
            conn.execute(
                "UPDATE thread_index_request_targets SET work_generation = ?3 \
                 WHERE request_id = ?1 AND run_id = ?2",
                params![request_id, run_id, generation],
            )?;
        }
        Outcome::Succeeded => {
            conn.execute(
                "UPDATE thread_index_request_targets SET state = 'succeeded', gap_reason = NULL \
                 WHERE request_id = ?1 AND run_id = ?2",
                params![request_id, run_id],
            )?;
        }
        Outcome::Gap(reason) => {
            conn.execute(
                "UPDATE thread_index_request_targets SET state = 'gap', gap_reason = ?3 \
                 WHERE request_id = ?1 AND run_id = ?2",
                params![request_id, run_id, to_json(reason)?],
            )?;
        }
    }
    Ok(())
}

/// Settles pending targets of live requests from the run rows.
pub(crate) fn sync(conn: &Connection) -> Result<(), StorageError> {
    for (request_id, run_id, target, generation, run) in targets_with_runs(conn, "pending", None)? {
        let outcome = judge(&target, generation, run.as_ref(), false)?;
        set_target(conn, &request_id, &run_id, &outcome)?;
    }
    Ok(())
}

/// Every pending target of every live request becomes `reason` (capacity
/// pause / disabled index: requests never wait indefinitely).
pub(crate) fn gap_pending(
    conn: &Connection,
    reason: RequestGapReason,
) -> Result<usize, StorageError> {
    Ok(conn.execute(
        "UPDATE thread_index_request_targets SET state = 'gap', gap_reason = ?1 \
         WHERE state = 'pending' AND request_id IN \
         (SELECT request_id FROM thread_index_requests WHERE state = 'pending')",
        [to_json(&reason)?],
    )?)
}

/// Pending targets of live requests whose bounded WAL wait is over become
/// `reason`. The wait of one request started at `max(since_ns, created_at_ns)`
/// (a request submitted during the wait waits from its own submission), so a
/// request is expired when that instant plus `limit_ns` is `<= now_ns`.
pub(crate) fn gap_expired(
    conn: &Connection,
    since_ns: i64,
    limit_ns: i64,
    now_ns: i64,
    reason: RequestGapReason,
) -> Result<usize, StorageError> {
    // Compare against `now - limit` so no sum can overflow.
    let started_by = now_ns.saturating_sub(limit_ns);
    Ok(conn.execute(
        "UPDATE thread_index_request_targets SET state = 'gap', gap_reason = ?1 \
         WHERE state = 'pending' AND request_id IN \
         (SELECT request_id FROM thread_index_requests \
          WHERE state = 'pending' AND max(?2, created_at_ns) <= ?3)",
        params![to_json(&reason)?, since_ns, started_by],
    )?)
}

/// The earliest wait deadline among live requests that still have a pending
/// target; `None` when nothing waits. Cancelled and finished requests are not
/// considered.
pub(crate) fn earliest_wait_deadline(
    conn: &Connection,
    since_ns: i64,
    limit_ns: i64,
) -> Result<Option<i64>, StorageError> {
    let started: Option<i64> = conn.query_row(
        "SELECT MIN(max(?1, q.created_at_ns)) FROM thread_index_requests q \
         WHERE q.state = 'pending' AND EXISTS (SELECT 1 FROM thread_index_request_targets t \
         WHERE t.request_id = q.request_id AND t.state = 'pending')",
        [since_ns],
        |row| row.get(0),
    )?;
    Ok(started.map(|started| started.saturating_add(limit_ns)))
}

/// The newest `created_at_ns` among live requests that still have a pending
/// target. Cancelled and finished requests are not considered.
pub(crate) fn latest_pending_created_at(conn: &Connection) -> Result<Option<i64>, StorageError> {
    Ok(conn.query_row(
        "SELECT MAX(q.created_at_ns) FROM thread_index_requests q \
         WHERE q.state = 'pending' AND EXISTS (SELECT 1 FROM thread_index_request_targets t \
         WHERE t.request_id = q.request_id AND t.state = 'pending')",
        [],
        |row| row.get(0),
    )?)
}

pub(super) const HAS_PENDING_TARGETS_SQL: &str = "SELECT EXISTS(SELECT 1 FROM thread_index_requests q \
     WHERE q.state = 'pending' AND EXISTS (SELECT 1 FROM thread_index_request_targets t \
     WHERE t.request_id = q.request_id AND t.state = 'pending'))";

/// Heartbeats need only existence, not the latest timestamp across all work.
pub(crate) fn has_pending_targets(conn: &Connection) -> Result<bool, StorageError> {
    Ok(conn.query_row(HAS_PENDING_TARGETS_SQL, [], |row| row.get(0))?)
}

/// Terminalizes requests without pending targets after re-checking that
/// succeeded targets are still held at their frozen revision/versions.
pub(crate) fn finalize(conn: &Connection, now_ns: i64) -> Result<usize, StorageError> {
    let mut statement = conn.prepare(
        "SELECT request_id FROM thread_index_requests q WHERE state = 'pending' \
         AND NOT EXISTS (SELECT 1 FROM thread_index_request_targets t \
         WHERE t.request_id = q.request_id AND t.state = 'pending')",
    )?;
    let ready = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    if ready.is_empty() {
        return Ok(0);
    }
    // Only the requests that are ready: the succeeded targets of the others are not read.
    for ready_id in &ready {
        for (request_id, run_id, target, generation, run) in
            targets_with_runs(conn, "succeeded", Some(ready_id))?
        {
            let outcome = judge(&target, generation, run.as_ref(), true)?;
            if !matches!(outcome, Outcome::Succeeded) {
                set_target(conn, &request_id, &run_id, &outcome)?;
            }
        }
    }
    for request_id in &ready {
        let gaps: u64 = conn.query_row(
            "SELECT COUNT(*) FROM thread_index_request_targets WHERE request_id = ?1 \
             AND state = 'gap'",
            [request_id],
            |row| row.get(0),
        )?;
        conn.execute(
            "UPDATE thread_index_requests SET state = ?2, finished_at_ns = ?3 \
             WHERE request_id = ?1 AND state = 'pending'",
            params![
                request_id,
                if gaps == 0 {
                    "completed"
                } else {
                    "completed_with_gaps"
                },
                now_ns
            ],
        )?;
    }
    Ok(ready.len())
}

pub(crate) fn cancel(
    conn: &Connection,
    request_id: &str,
    now_ns: i64,
) -> Result<bool, StorageError> {
    Ok(conn.execute(
        "UPDATE thread_index_requests SET state = 'cancelled', finished_at_ns = ?2 \
         WHERE request_id = ?1 AND state = 'pending'",
        params![request_id, now_ns],
    )? == 1)
}

fn summary(
    conn: &Connection,
    request_id: &str,
    deleted: u64,
) -> Result<RequestSummary, StorageError> {
    let mut statement = conn.prepare(
        "SELECT state, gap_reason, COUNT(*) FROM thread_index_request_targets \
         WHERE request_id = ?1 GROUP BY state, gap_reason",
    )?;
    let rows = statement
        .query_map([request_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, u64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut target_count = 0;
    let mut succeeded = 0;
    let mut gaps: BTreeMap<String, (RequestGapReason, u64)> = BTreeMap::new();
    for (state, reason, count) in rows {
        target_count += count;
        match (state.as_str(), reason) {
            ("succeeded", _) => succeeded += count,
            ("gap", Some(reason)) => {
                let parsed: RequestGapReason = from_json(&reason)?;
                gaps.entry(reason).or_insert((parsed, 0)).1 += count;
            }
            _ => {}
        }
    }
    Ok(RequestSummary {
        target_count,
        succeeded,
        deleted_before_submission: deleted,
        gaps: gaps
            .into_values()
            .map(|(reason, count)| RequestGapCount { reason, count })
            .collect(),
    })
}

/// Only the current fenced worker may fail the shared pending requests.
pub(crate) fn fail_pending(
    conn: &Connection,
    held: &crate::thread_index::IndexLease,
    now_ns: i64,
) -> Result<(), StorageError> {
    let transaction =
        rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    if lease::is_valid(&transaction, held, now_ns)? {
        transaction.execute(
            "UPDATE thread_index_requests SET state = 'failed', failure_reason = ?1, \
             finished_at_ns = ?2 WHERE state = 'pending'",
            params![
                serde_json::to_string(&RequestFailure::UnrecoverableControlFailure)
                    .map_err(|error| StorageError::Serialization(error.to_string()))?,
                now_ns
            ],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

/// Shared GUI/CLI status, including whether any owner currently holds the lease.
///
/// A request that is still `registering` (its targets are being committed in batches) is
/// not observable: it is reported as unknown, never as cancelled.
pub(crate) fn status(
    conn: &Connection,
    request_id: &str,
    now_ns: i64,
) -> Result<Option<RequestStatus>, StorageError> {
    let row: Option<(String, Option<String>, u64)> = conn
        .query_row(
            "SELECT state, failure_reason, deleted_before_submission FROM thread_index_requests \
             WHERE request_id = ?1 AND state <> 'registering'",
            [request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((state, failure, deleted)) = row else {
        return Ok(None);
    };
    let summary = summary(conn, request_id, deleted)?;
    let id = request_id.to_owned();
    let progress = match state.as_str() {
        "pending" => RequestProgress::Pending(summary),
        "completed" => RequestProgress::Terminal(ReindexResult::Completed {
            request_id: id,
            summary,
        }),
        "completed_with_gaps" => RequestProgress::Terminal(ReindexResult::CompletedWithGaps {
            request_id: id,
            summary,
        }),
        "failed" => RequestProgress::Terminal(ReindexResult::Failed {
            request_id: Some(id),
            reason: failure
                .as_deref()
                .map(from_json)
                .transpose()?
                .unwrap_or(RequestFailure::UnrecoverableControlFailure),
        }),
        _ => RequestProgress::Cancelled,
    };
    Ok(Some(RequestStatus {
        request_id: request_id.to_owned(),
        progress,
        owner: lease::live(conn, now_ns)?,
    }))
}

/// Test helper: the whole registration through the bounded steps, on a plain
/// connection (each step is its own transaction).
#[cfg(test)]
pub(crate) fn submit_for_test(
    conn: &Connection,
    request: &ReindexRequest,
    now_ns: i64,
) -> Result<(String, u64), StorageError> {
    let request_id = begin_registration(conn, request, now_ns)?;
    let mut cursor = None;
    let mut targets = 0;
    loop {
        let batch = freeze_batch(conn, &request_id, cursor.as_ref(), FREEZE_BATCH)?;
        targets += batch.targets;
        cursor = batch.next;
        if cursor.is_none() {
            break;
        }
    }
    immediate(conn, |conn| publish_registration(conn, &request_id, now_ns))?;
    Ok((request_id, targets))
}
