//! The usage of one thread or one run, read from the ledger rows of its runs
//! (ADR 0045 E4). The reduction is the summary's own ([`add_request`]), so a
//! request counts the same here as in the usage tab.
//!
//! A thread is its runs: every run `runs_of_thread` lists, whichever of them
//! started the conversation, continued it or was started by one of those. The
//! ledger's own run-to-thread attribution is not used (a request carries its
//! run, and the attribution is written late). Costs are the recorded ones.
use super::{add_request, merge};
use crate::queries::{error, storage_error};
use std::collections::BTreeSet;
use storage::{Database, usage::UsageRequestRow};
use workbench_protocol::{
    errors::{ErrorCode, WorkbenchError},
    ids::{RunId, ThreadId},
    namespaces::diagnostics::{RunUsage, ThreadUsage, UsageTotals, UsageValue},
};

const DAY_NS: i64 = 86_400 * 1_000_000_000;

/// Whether requests of something created at `created_at_ns` (`None` when that is
/// unknown) may have been rolled up into days by now. A rolled-up request keeps
/// neither its run nor its thread, so the rows that are left are a lower bound.
/// A retention of 0 days turns the roll-up off: nothing leaves the ledger.
pub(super) fn may_be_rolled_up(
    created_at_ns: Option<i64>,
    now_ns: i64,
    retention_days: u32,
) -> bool {
    if retention_days == 0 {
        return false;
    }
    let kept = i64::from(retention_days).saturating_mul(DAY_NS);
    created_at_ns.is_none_or(|created| created < now_ns.saturating_sub(kept))
}

fn totals_of<'a>(rows: impl IntoIterator<Item = &'a UsageRequestRow>) -> UsageTotals {
    let mut totals = UsageTotals::default();
    for row in rows {
        let mut one = UsageTotals::default();
        add_request(&mut one, row);
        merge(&mut totals, &one);
    }
    totals
}

/// The usage of `thread`. A thread without a row and without runs is unknown; one
/// that has runs but no row cannot say when it began, so its usage is incomplete
/// (unless the ledger keeps every request).
pub(super) fn thread_usage(
    database: &Database,
    thread: &ThreadId,
    now_ns: i64,
    retention_days: u32,
) -> Result<ThreadUsage, WorkbenchError> {
    let runs = database
        .runs_of_thread(thread.as_str())
        .map_err(storage_error)?;
    let all: BTreeSet<&str> = runs.iter().map(|run| run.run_id.as_str()).collect();
    let roots: BTreeSet<&str> = runs
        .iter()
        .filter(|run| run.depth == 0)
        .map(|run| run.run_id.as_str())
        .collect();
    let rows = if all.is_empty() {
        Vec::new()
    } else {
        let ids: Vec<String> = all.iter().map(|id| (*id).to_owned()).collect();
        database
            .usage_requests_for_runs(&ids)
            .map_err(storage_error)?
    };
    let created_at_ns = database
        .thread(thread.as_str())
        .map_err(storage_error)?
        .map(|row| row.created_at_ns);
    if created_at_ns.is_none() && all.is_empty() {
        return Err(error(ErrorCode::NotFound, "unknown thread"));
    }
    let incomplete = may_be_rolled_up(created_at_ns, now_ns, retention_days);
    let in_roots = |row: &&UsageRequestRow| {
        row.record
            .run_id
            .as_deref()
            .is_some_and(|run| roots.contains(run))
    };
    Ok(ThreadUsage {
        thread: thread.clone(),
        total: UsageValue {
            totals: totals_of(&rows),
            incomplete,
        },
        conversation: UsageValue {
            totals: totals_of(rows.iter().filter(in_roots)),
            incomplete,
        },
    })
}

/// The usage of `run` alone. A run with no request, no thread and no saved context
/// is unknown; one whose thread is unknown cannot say when it began, so its usage
/// is incomplete (unless the ledger keeps every request).
pub(super) fn run_usage(
    database: &Database,
    run: &RunId,
    now_ns: i64,
    retention_days: u32,
) -> Result<RunUsage, WorkbenchError> {
    let ids = [run.as_str().to_owned()];
    let rows = database
        .usage_requests_for_runs(&ids)
        .map_err(storage_error)?;
    let placement = database
        .thread_of_run(run.as_str())
        .map_err(storage_error)?;
    let created_at_ns = match placement {
        Some(placement) => database
            .thread(&placement.thread_id)
            .map_err(storage_error)?
            .map(|thread| thread.created_at_ns),
        None => None,
    };
    let saved = database
        .run_context_parent(run.as_str())
        .map_err(storage_error)?
        .is_some();
    if rows.is_empty() && created_at_ns.is_none() && !saved {
        return Err(error(ErrorCode::NotFound, "unknown run"));
    }
    let incomplete = may_be_rolled_up(created_at_ns, now_ns, retention_days);
    Ok(RunUsage {
        run: run.clone(),
        total: UsageValue {
            totals: totals_of(&rows),
            incomplete,
        },
    })
}
