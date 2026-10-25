//! `evorch reindex-threads`: the same durable request/lease/step path as the GUI.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use storage::thread_index::{
    IndexLease, IndexOwner, IndexOwnerKind, IndexStepResult, LeaseResult, ReindexRequest,
    ReindexResult, RequestFailure, RequestProgress, RequestSubmission, ThreadIndexSettings,
};
use storage::{Storage, StorageConfig, StorageHandle, THREAD_INDEX_LEASE_NS};

pub const USAGE: &str = "usage: evorch reindex-threads [--db <path>] [--project <dir>] [--since <unix-ns>] [--include-evicted] [--wait]
--since is an inclusive, non-negative Unix nanosecond timestamp.
The default database is <user-config-dir>/evorch-events.db, the GUI's store.
Output is JSON: accepted, completed, completed_with_gaps, or failed.
accepted delegates a durable request to another owner; it does not mean completed.";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReindexArgs {
    pub db: Option<PathBuf>,
    pub project: Option<PathBuf>,
    pub since_ns: Option<i64>,
    pub include_evicted: bool,
    pub wait: bool,
}

/// Parses everything after `evorch`, starting with `reindex-threads`.
pub fn parse_args(mut argv: impl Iterator<Item = String>) -> Result<ReindexArgs, String> {
    if argv.next().as_deref() != Some("reindex-threads") {
        return Err(USAGE.into());
    }
    let mut args = ReindexArgs::default();
    let mut seen = std::collections::BTreeSet::new();
    while let Some(arg) = argv.next() {
        if !seen.insert(arg.clone()) {
            return Err(format!("duplicate argument: {arg}\n{USAGE}"));
        }
        match arg.as_str() {
            "--wait" => args.wait = true,
            "--include-evicted" => args.include_evicted = true,
            "--db" | "--project" | "--since" => {
                let value = argv
                    .next()
                    .filter(|value| !value.is_empty() && !value.starts_with("--"))
                    .ok_or(USAGE)?;
                match arg.as_str() {
                    "--db" => args.db = Some(PathBuf::from(value)),
                    "--project" => args.project = Some(PathBuf::from(value)),
                    _ => {
                        args.since_ns = Some(
                            value
                                .parse::<i64>()
                                .ok()
                                .filter(|value| *value >= 0)
                                .ok_or_else(|| format!("invalid --since\n{USAGE}"))?,
                        );
                    }
                }
            }
            _ => return Err(USAGE.into()),
        }
    }
    Ok(args)
}

#[derive(Debug)]
pub struct ReindexError {
    pub reason: RequestFailure,
    pub message: String,
}

/// External cancellation stops waiting, but is not a persisted failed result.
#[derive(Debug)]
pub enum ReindexOutcome {
    Result(ReindexResult),
    Cancelled { request_id: String },
}

fn invalid(message: impl ToString) -> ReindexError {
    ReindexError {
        reason: RequestFailure::InvalidArguments,
        message: message.to_string(),
    }
}

fn request(args: &ReindexArgs) -> Result<ReindexRequest, ReindexError> {
    let canonical_project = args
        .project
        .as_ref()
        .map(|path| {
            let path = path.canonicalize().map_err(invalid)?;
            if !path.is_dir() {
                return Err(invalid("--project must be a directory"));
            }
            path.into_os_string()
                .into_string()
                .map_err(|_| invalid("--project must be UTF-8"))
        })
        .transpose()?;
    if args.since_ns.is_some_and(|since| since < 0) {
        return Err(invalid("--since must be non-negative Unix nanoseconds"));
    }
    Ok(ReindexRequest {
        canonical_project,
        since_ns: args.since_ns,
        include_evicted: args.include_evicted,
    })
}

/// Opens the selected store and runs a foreground request, never a hidden daemon.
/// No SQL or independent indexing logic belongs to this command.
pub fn run(args: ReindexArgs) -> Result<ReindexOutcome, ReindexError> {
    run_with_options(args, &config::LoadOptions::default())
}

fn run_with_options(
    args: ReindexArgs,
    options: &config::LoadOptions,
) -> Result<ReindexOutcome, ReindexError> {
    let request = request(&args)?;
    let config = config::Config::load(options).map_err(invalid)?;
    let db_path = args
        .db
        .or_else(|| config::user_config_dir().map(|dir| dir.join("evorch-events.db")))
        .ok_or_else(|| invalid("cannot resolve user config dir; pass --db"))?;
    if !db_path.is_file() {
        return Err(ReindexError {
            reason: RequestFailure::DatabaseUnavailable,
            message: format!("database does not exist: {}", db_path.display()),
        });
    }
    let index = &config.storage.thread_index;
    let storage = Storage::open(StorageConfig {
        db_path,
        thread_index: ThreadIndexSettings {
            enabled: index.enabled,
            max_bytes: index.max_bytes,
        },
        ..StorageConfig::default()
    })
    .map_err(|error| ReindexError {
        reason: RequestFailure::DatabaseUnavailable,
        message: error.to_string(),
    })?;
    let owner = IndexOwner {
        id: format!("cli-{}-{}", std::process::id(), now_ns()),
        kind: IndexOwnerKind::Cli,
    };
    Ok(execute(
        &storage.handle(),
        &request,
        &owner,
        args.wait,
        now_ns,
        std::thread::sleep,
    ))
}

fn now_ns() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    )
    .unwrap_or(i64::MAX)
}

fn failed(request_id: Option<String>, reason: RequestFailure) -> ReindexResult {
    ReindexResult::Failed { request_id, reason }
}

/// The wait and clock are injected so tests advance work/leases with no sleeps.
fn execute(
    handle: &StorageHandle,
    request: &ReindexRequest,
    owner: &IndexOwner,
    wait: bool,
    mut clock: impl FnMut() -> i64,
    mut pause: impl FnMut(Duration),
) -> ReindexOutcome {
    let submission =
        match handle.submit_thread_index_request(request, owner, clock(), THREAD_INDEX_LEASE_NS) {
            Ok(submission) => submission,
            Err(reason) => return ReindexOutcome::Result(failed(None, reason)),
        };
    let RequestSubmission {
        request_id,
        lease,
        progress,
    } = submission;
    let mut lease = match lease {
        LeaseResult::Acquired(lease) => Some(lease),
        LeaseResult::HeldByOther(_) if !wait && matches!(progress, RequestProgress::Pending(_)) => {
            return ReindexOutcome::Result(ReindexResult::Accepted { request_id });
        }
        _ => None,
    };
    let outcome = drive(
        handle,
        owner,
        &request_id,
        progress,
        &mut lease,
        &mut clock,
        &mut pause,
    );
    // Releasing is fenced, so a lost lease cannot release its successor.
    if let Some(lease) = lease
        && let Err(reason) = handle.release_thread_index_lease(&lease)
    {
        // Cleanup cannot rewrite an already persisted terminal outcome.
        eprintln!("request {request_id}: lease release failed: {reason:?}");
    }
    outcome
}

fn drive(
    handle: &StorageHandle,
    owner: &IndexOwner,
    request_id: &str,
    mut progress: RequestProgress,
    lease: &mut Option<IndexLease>,
    clock: &mut impl FnMut() -> i64,
    pause: &mut impl FnMut(Duration),
) -> ReindexOutcome {
    let result = (|| {
        let mut backoff = Duration::from_millis(25);
        loop {
            match progress {
                RequestProgress::Terminal(result) => return Ok(Some(result)),
                RequestProgress::Cancelled => return Ok(None),
                RequestProgress::Pending(_) => {}
            }
            let now = clock();
            let mut next_due = None;
            if let Some(current) = lease {
                if current.expires_at_ns.saturating_sub(now) <= THREAD_INDEX_LEASE_NS / 2 {
                    *lease = match handle.heartbeat_thread_index_lease(
                        current,
                        now,
                        THREAD_INDEX_LEASE_NS,
                    )? {
                        LeaseResult::Acquired(renewed) => Some(renewed),
                        _ => None,
                    };
                }
            } else {
                *lease =
                    match handle.acquire_thread_index_lease(owner, now, THREAD_INDEX_LEASE_NS)? {
                        LeaseResult::Acquired(acquired) => Some(acquired),
                        LeaseResult::HeldByOther(other) => {
                            next_due = Some(other.expires_at_ns);
                            None
                        }
                        LeaseResult::Lost => None,
                    };
            }
            let mut made_progress = false;
            if let Some(current) = lease.as_ref() {
                match handle.step_thread_index(current, now)? {
                    IndexStepResult::Progress => made_progress = true,
                    IndexStepResult::Idle { next_wake_at_ns } => next_due = next_wake_at_ns,
                    IndexStepResult::Paused(_) => {}
                    IndexStepResult::LeaseLost => *lease = None,
                }
            }
            let status = handle.thread_index_request_status(request_id, clock())?;
            progress = status.progress;
            if matches!(progress, RequestProgress::Pending(_)) && !made_progress {
                let now = clock();
                let until_due = next_due
                    .map(|due| {
                        Duration::from_nanos(u64::try_from(due.saturating_sub(now)).unwrap_or(0))
                    })
                    .unwrap_or(backoff);
                // Cap polling latency at a second; never spin on an overdue
                // wake, and wake well before our lease needs a heartbeat.
                pause(backoff.min(until_due).max(Duration::from_millis(1)));
                backoff = (backoff * 2).min(Duration::from_secs(1));
            } else {
                backoff = Duration::from_millis(25);
            }
        }
    })();
    match result {
        Ok(Some(result)) => ReindexOutcome::Result(result),
        Ok(None) => ReindexOutcome::Cancelled {
            request_id: request_id.to_owned(),
        },
        Err(reason) => ReindexOutcome::Result(failed(Some(request_id.to_owned()), reason)),
    }
}

#[cfg(test)]
mod tests;
