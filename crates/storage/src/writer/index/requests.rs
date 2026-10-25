//! Persisted reindex requests (ADR 0041 R5).
//!
//! Submission freezes `(run, revision, versions)` targets in one transaction and
//! applies the explicit-reindex R2 cell. Targets are then settled from the run
//! rows (`sync`): success needs the same revision/versions committed; every
//! other terminal state is a reasoned gap. A request terminalizes when no
//! target is pending, after re-checking that succeeded targets are still held.
//! Terminal results never change afterwards.

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
             include_evicted, state, created_at_ns) VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
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
pub(crate) fn rebind_shared(conn: &Connection, run_id: &str) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE thread_index_request_targets AS t SET work_generation = r.work_generation \
         FROM thread_index_runs AS r \
         WHERE r.run_id = t.run_id AND t.run_id = ?1 AND t.state = 'pending' \
         AND t.source_revision = r.source_revision \
         AND t.extractor_version = r.extractor_version \
         AND t.accounting_version = r.accounting_version \
         AND t.work_generation <> r.work_generation \
         AND t.request_id IN (SELECT request_id FROM thread_index_requests WHERE state = 'pending')",
        [run_id],
    )?;
    Ok(())
}

/// Inserts the request and its targets inside the caller's transaction.
/// Returns the request id and the number of materialized targets.
pub(crate) fn submit_in(
    conn: &Connection,
    request: &ReindexRequest,
    now_ns: i64,
) -> Result<(String, u64), StorageError> {
    let request_id = insert_request(conn, request, now_ns)?;
    let mut statement = conn.prepare(
        "SELECT r.run_id, r.state FROM thread_index_runs r \
         WHERE (?1 IS NULL OR r.source_updated_at_ns >= ?1) \
         AND (?2 IS NULL OR EXISTS (SELECT 1 FROM thread_runs tr \
              JOIN threads t ON t.thread_id = tr.thread_id \
              WHERE tr.run_id = r.run_id AND t.project_root = ?2)) \
         ORDER BY r.retention_at_ns DESC, r.run_id DESC",
    )?;
    let candidates = statement
        .query_map(
            params![request.since_ns, request.canonical_project],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut deleted = 0_u64;
    let mut targets = 0_u64;
    for (run_id, state) in candidates {
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
    conn.execute(
        "UPDATE thread_index_requests SET deleted_before_submission = ?2 WHERE request_id = ?1",
        params![request_id, deleted],
    )?;
    if targets == 0 {
        conn.execute(
            "UPDATE thread_index_requests SET state = 'completed', finished_at_ns = ?2 \
             WHERE request_id = ?1",
            params![request_id, now_ns],
        )?;
    }
    Ok((request_id, targets))
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

fn targets_with_runs(conn: &Connection, target_state: &str) -> Result<Vec<Judged>, StorageError> {
    let mut statement = conn.prepare(&format!(
        "SELECT t.request_id, t.run_id, t.source_revision, t.extractor_version, \
         t.accounting_version, t.work_generation, {RUN_COLUMNS} \
         FROM thread_index_request_targets t \
         JOIN thread_index_requests q ON q.request_id = t.request_id AND q.state = 'pending' \
         LEFT JOIN thread_index_runs r ON r.run_id = t.run_id WHERE t.state = ?1"
    ))?;
    let rows = statement
        .query_map([target_state], |row| {
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
    for (request_id, run_id, target, generation, run) in targets_with_runs(conn, "pending")? {
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
    for (request_id, run_id, target, generation, run) in targets_with_runs(conn, "succeeded")? {
        if !ready.contains(&request_id) {
            continue;
        }
        let outcome = judge(&target, generation, run.as_ref(), true)?;
        if !matches!(outcome, Outcome::Succeeded) {
            set_target(conn, &request_id, &run_id, &outcome)?;
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

/// Shared GUI/CLI status, including whether any owner currently holds the lease.
pub(crate) fn status(
    conn: &Connection,
    request_id: &str,
    now_ns: i64,
) -> Result<Option<RequestStatus>, StorageError> {
    let row: Option<(String, Option<String>, u64)> = conn
        .query_row(
            "SELECT state, failure_reason, deleted_before_submission FROM thread_index_requests \
             WHERE request_id = ?1",
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
