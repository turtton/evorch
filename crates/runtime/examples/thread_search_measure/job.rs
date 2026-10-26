use std::path::Path;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use storage::thread_index::{
    IndexCapacityStatus, IndexLease, IndexOwner, IndexOwnerKind, IndexStepResult, LeaseResult,
    PlannedGrowth, ReindexRequest, ReindexResult, RequestProgress, RequestSummary,
};
use storage::{Database, StorageHandle, THREAD_INDEX_LEASE_NS};

use super::{DEADLINE, Result, corpus::Corpus, driver::Sample, require};

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64
}

fn failure<T>(result: std::result::Result<T, impl std::fmt::Debug>) -> Result<T> {
    result.map_err(|_| "index_control_error".into())
}

fn owner() -> IndexOwner {
    IndexOwner {
        id: "s8-benchmark-foreground".into(),
        kind: IndexOwnerKind::Cli,
    }
}

fn acquired(result: LeaseResult) -> Result<IndexLease> {
    match result {
        LeaseResult::Acquired(lease) => Ok(lease),
        _ => Err("index_lease_unavailable".into()),
    }
}

fn heartbeat(handle: &StorageHandle, lease: &mut IndexLease) -> Result<()> {
    *lease = acquired(failure(handle.heartbeat_thread_index_lease(
        lease,
        now(),
        THREAD_INDEX_LEASE_NS,
    ))?)?;
    Ok(())
}

/// Admission evidence travels with the exact Paused result. Never replace it
/// with a later capacity query (the bound/operation may already have changed).
#[derive(Debug)]
struct EvidenceError {
    code: String,
    evidence: Value,
}

impl std::fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.code)
    }
}
impl std::error::Error for EvidenceError {}

pub fn error_evidence(error: &(dyn std::error::Error + 'static)) -> Value {
    error
        .downcast_ref::<EvidenceError>()
        .map(|error| error.evidence.clone())
        .unwrap_or_else(|| json!({}))
}

fn with_evidence(
    error: Box<dyn std::error::Error + Send + Sync>,
    key: &str,
    value: Value,
) -> Box<dyn std::error::Error + Send + Sync> {
    let mut evidence = error_evidence(error.as_ref());
    evidence[key] = value;
    Box::new(EvidenceError {
        code: super::error_class(error.as_ref()),
        evidence,
    })
}

pub fn paused_error(status: IndexCapacityStatus) -> Box<dyn std::error::Error + Send + Sync> {
    let growth = match status.growth {
        PlannedGrowth::Bounded { db, wal, shm } => json!({"db":db, "wal":wal, "shm":shm}),
        PlannedGrowth::Unbounded => json!({"unbounded":true}),
    };
    Box::new(EvidenceError {
        code: "index_capacity_paused".into(),
        evidence: json!({"paused": {
            "reason":format!("{:?}", status.state),
            "adjustment":format!("{:?}", status.adjustment),
            "db_bytes":status.db_bytes, "wal_bytes":status.wal_bytes, "shm_bytes":status.shm_bytes,
            "reserve_bytes":status.reserve_bytes, "resume_hysteresis_bytes":status.resume_hysteresis_bytes,
            "operation":format!("{:?}", status.operation), "growth":growth,
            "max_db_bytes":status.max_db_bytes, "max_wal_bytes":status.max_wal_bytes,
        }}),
    })
}

fn step(handle: &StorageHandle, lease: &IndexLease) -> Result<()> {
    match failure(handle.step_thread_index(lease, now()))? {
        IndexStepResult::Progress => Ok(()),
        // Extraction is asynchronous on the shared worker for CLI and GUI.
        IndexStepResult::Idle { .. } => {
            thread::sleep(Duration::from_millis(1));
            Ok(())
        }
        IndexStepResult::Paused(status) => Err(paused_error(status)),
        IndexStepResult::LeaseLost => Err("index_lease_lost".into()),
    }
}

fn submit(handle: &StorageHandle, project: String) -> Result<String> {
    let submission = failure(handle.submit_thread_index_request(
        &ReindexRequest {
            canonical_project: Some(project),
            since_ns: None,
            include_evicted: true,
        },
        &owner(),
        now(),
        THREAD_INDEX_LEASE_NS,
    ))?;
    require(
        matches!(submission.lease, LeaseResult::Acquired(_)),
        "request_lease_unavailable",
    )?;
    Ok(submission.request_id)
}

fn summary(handle: &StorageHandle, id: &str) -> Result<(RequestSummary, bool)> {
    let status = failure(handle.thread_index_request_status(id, now()))?;
    match status.progress {
        RequestProgress::Pending(summary) => Ok((summary, false)),
        RequestProgress::Terminal(
            ReindexResult::Completed { summary, .. }
            | ReindexResult::CompletedWithGaps { summary, .. },
        ) => Ok((summary, true)),
        _ => Err("request_failed_or_cancelled".into()),
    }
}

pub fn preindex(handle: &StorageHandle, db: &Database, corpus: &Corpus) -> Result<()> {
    preindex_project(
        handle,
        db,
        &corpus.source_runs,
        &corpus.threads[0]
            .project_root
            .clone()
            .ok_or("missing_seed_project")?,
    )?;
    corpus.verify_seed(db)
}

pub fn preindex_project(
    handle: &StorageHandle,
    db: &Database,
    sources: &std::collections::BTreeSet<String>,
    project: &str,
) -> Result<()> {
    let mut lease = acquired(failure(handle.acquire_thread_index_lease(
        &owner(),
        now(),
        THREAD_INDEX_LEASE_NS,
    ))?)?;
    let started = Instant::now();
    // Discovery must precede request submission: an empty target list is not a
    // successful seed. Stop before the unrequested fallback consumes all work.
    loop {
        heartbeat(handle, &mut lease)?;
        if sources
            .iter()
            .all(|run| db.thread_index_run_status(run).ok().flatten().is_some())
        {
            break;
        }
        require(
            started.elapsed() < Duration::from_secs(1800),
            "discovery_deadline",
        )?;
        step(handle, &lease)?;
    }
    // Legacy discovery/proof can legitimately replace a request's frozen
    // revision. Re-request the current revision, bounded to four passes; never
    // mutate targets or reinterpret superseded as successful indexing.
    for pass in 0..4 {
        let request = submit(handle, project.to_owned())?;
        loop {
            heartbeat(handle, &mut lease)?;
            if summary(handle, &request)?.1 {
                break;
            }
            require(
                started.elapsed() < Duration::from_secs(1800),
                "seed_deadline",
            )?;
            step(handle, &lease)?;
        }
        let (result, _) = summary(handle, &request)?;
        println!(
            "{}",
            json!({"stage":"seed_request", "pass":pass, "summary":result})
        );
        let superseded = result.gaps.iter().any(|gap| {
            matches!(
                gap.reason,
                storage::thread_index::RequestGapReason::Superseded
                    | storage::thread_index::RequestGapReason::VersionSuperseded
            )
        });
        if !superseded {
            break;
        }
        require(pass < 3, "seed_revision_not_stable")?;
    }
    failure(handle.release_thread_index_lease(&lease))?;
    Ok(())
}

fn totals(handle: &StorageHandle, ids: &[String]) -> Result<(u64, u64, u64)> {
    let mut total = 0;
    let mut succeeded = 0;
    let mut pending = 0;
    for id in ids {
        let (summary, _) = summary(handle, id)?;
        total += summary.target_count;
        succeeded += summary.succeeded;
        let gaps = summary.gaps.iter().map(|gap| gap.count).sum::<u64>();
        pending += summary
            .target_count
            .saturating_sub(summary.succeeded + gaps);
    }
    Ok((total, succeeded, pending))
}

struct Progress {
    before: (u64, u64, u64),
    after: (u64, u64, u64),
    started: Instant,
    ended: Instant,
    steps: u64,
}

enum Command {
    Arm(
        mpsc::Receiver<Instant>,
        mpsc::Sender<Result<Progress>>,
        mpsc::Sender<()>,
    ),
    Close,
}

pub struct Parallel {
    commands: mpsc::Sender<Command>,
    job: Option<JoinHandle<Result<()>>>,
    receive: Option<mpsc::Receiver<Result<Progress>>>,
}

impl Parallel {
    pub fn new(handle: StorageHandle, projects: &[String]) -> Result<Self> {
        let mut lease = acquired(failure(handle.acquire_thread_index_lease(
            &owner(),
            now(),
            THREAD_INDEX_LEASE_NS,
        ))?)?;
        let ids = projects
            .iter()
            .cloned()
            .map(|project| submit(&handle, project))
            .collect::<Result<Vec<_>>>()?;
        let projects = projects.to_vec();
        require(
            totals(&handle, &ids)?.2 >= super::REPETITIONS as u64,
            "insufficient_backfill_targets",
        )?;
        let (commands, receive) = mpsc::channel();
        let job = thread::Builder::new()
            .name("s8-shared-index-steps".into())
            .spawn(move || {
                let mut ids = ids;
                let mut revision_passes = 0;
                loop {
                    let command = match receive.recv_timeout(Duration::from_secs(10)) {
                        Ok(command) => command,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            heartbeat(&handle, &mut lease)?;
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    let Command::Arm(trigger, done, ready) = command else {
                        break;
                    };
                    heartbeat(&handle, &mut lease)?;
                    let before = totals(&handle, &ids)?;
                    let mut started = Instant::now();
                    let mut steps = 0;
                    let mut after = before;
                    let result = (|| {
                        require(before.2 > 0, "backfill_exhausted")?;
                        let _ = ready.send(());
                        trigger
                            .recv_timeout(DEADLINE)
                            .map_err(|_| "backfill_trigger_timeout")?;
                        started = Instant::now();
                        loop {
                            require(started.elapsed() < DEADLINE, "backfill_progress_deadline")?;
                            heartbeat(&handle, &mut lease)?;
                            let outcome = step(&handle, &lease);
                            steps += 1;
                            // Even the final (paused/failed) step can terminalize
                            // targets. Preserve progress before propagating it.
                            let measured = totals(&handle, &ids);
                            if let Ok(totals) = &measured {
                                after = *totals;
                            }
                            outcome?;
                            // A progress-read failure must still fail the cell;
                            // fallback counters only preserve failure evidence.
                            after = measured?;
                            if after.1 > before.1 {
                                return Ok(Progress {
                                    before,
                                    after,
                                    started,
                                    ended: Instant::now(),
                                    steps,
                                });
                            }
                            if after.2 == 0 {
                                let superseded = ids.iter().any(|id| summary(&handle, id)
                                    .is_ok_and(|(summary, _)| summary.gaps.iter().any(|gap|
                                        matches!(gap.reason,
                                            storage::thread_index::RequestGapReason::Superseded
                                            | storage::thread_index::RequestGapReason::VersionSuperseded))));
                                require(superseded && revision_passes < 3,
                                    "backfill_exhausted_without_success")?;
                                ids = projects.iter().cloned()
                                    .map(|project| submit(&handle, project))
                                    .collect::<Result<Vec<_>>>()?;
                                revision_passes += 1;
                            }
                        }
                    })();
                    let result = result.map_err(|error| with_evidence(error, "backfill_attempt", json!({
                        "targets":before.0, "pending_before":before.2, "pending_after":after.2,
                        "succeeded_before":before.1, "succeeded_after":after.1,
                        "succeeded_delta":after.1.saturating_sub(before.1), "steps":steps,
                    })));
                    let failed = result.is_err();
                    let _ = done.send(result);
                    if failed {
                        break;
                    }
                }
                failure(handle.release_thread_index_lease(&lease))?;
                Ok(())
            })?;
        Ok(Self {
            commands,
            job: Some(job),
            receive: None,
        })
    }

    pub fn arm(&mut self) -> Result<mpsc::Sender<Instant>> {
        require(self.receive.is_none(), "backfill_already_armed")?;
        let (trigger, start) = mpsc::channel();
        let (done, receive) = mpsc::channel();
        let (ready, ready_receive) = mpsc::channel();
        self.commands
            .send(Command::Arm(start, done, ready))
            .map_err(|_| "backfill_worker_closed")?;
        ready_receive
            .recv_timeout(DEADLINE)
            .map_err(|_| "backfill_arm_timeout")?;
        self.receive = Some(receive);
        Ok(trigger)
    }

    pub fn finish(&mut self, repetition: usize, sample: &Sample) -> Result<Value> {
        let progress = self
            .receive
            .take()
            .ok_or("backfill_not_armed")?
            .recv_timeout(DEADLINE)
            .map_err(|_| "backfill_completion_timeout")??;
        let overlaps = progress.started < sample.finished && progress.ended > sample.started;
        let observed = json!({
            "repetition": repetition, "targets": progress.before.0,
            "pending_before": progress.before.2, "pending_after": progress.after.2,
            "succeeded_delta": progress.after.1 - progress.before.1,
            "steps": progress.steps, "overlap": overlaps,
            "background_us": progress.ended.duration_since(progress.started).as_micros(),
            "commit_observed_after_search": progress.ended > sample.finished,
        });
        require(
            overlaps && progress.steps > 0 && progress.after.1 > progress.before.1,
            "backfill_did_not_overlap",
        )
        .map_err(|error| with_evidence(error, "backfill_attempt", observed.clone()))?;
        Ok(observed)
    }

    /// Search validation can fail before `finish` is reached. Still collect the
    /// armed worker's last result so a simultaneous Paused is not discarded.
    pub fn search_failed(
        &mut self,
        error: Box<dyn std::error::Error + Send + Sync>,
    ) -> Box<dyn std::error::Error + Send + Sync> {
        let Some(receive) = self.receive.take() else {
            return error;
        };
        match receive.recv_timeout(DEADLINE) {
            Ok(Err(index_error)) => {
                let mut evidence = error_evidence(index_error.as_ref());
                evidence["search_error_class"] = json!(super::error_class(error.as_ref()));
                evidence["search_error_sha256"] =
                    json!(super::corpus::hash(error.to_string().as_bytes()));
                Box::new(EvidenceError {
                    code: super::error_class(index_error.as_ref()),
                    evidence,
                })
            }
            Ok(Ok(progress)) => with_evidence(
                error,
                "backfill_attempt",
                json!({
                    "targets":progress.before.0, "pending_before":progress.before.2, "pending_after":progress.after.2,
                    "succeeded_delta":progress.after.1.saturating_sub(progress.before.1), "steps":progress.steps,
                }),
            ),
            Err(_) => error,
        }
    }

    pub fn close(mut self) -> Result<()> {
        let _ = self.commands.send(Command::Close);
        self.job
            .take()
            .unwrap()
            .join()
            .map_err(|_| "backfill_worker_panicked")?
    }
}

impl Drop for Parallel {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Close);
        // No unbounded join in an error path. Normal close joins after each
        // measured drain's completion barrier, outside the timed interval.
    }
}

pub fn observation(handle: &StorageHandle, path: &Path) -> Result<Value> {
    let status = handle.thread_index_job_status()?;
    let capacity = handle.thread_index_capacity()?;
    let size = |suffix: &str| {
        std::fs::metadata(format!("{}{suffix}", path.display()))
            .map(|metadata| metadata.len())
            .unwrap_or(0)
    };
    Ok(json!({
        "db_bytes": size(""), "wal_bytes": size("-wal"), "shm_bytes": size("-shm"),
        "capacity": {
            "state": format!("{:?}", capacity.state),
            "db_bytes": capacity.db_bytes, "wal_bytes": capacity.wal_bytes,
            "shm_bytes": capacity.shm_bytes, "reserve_bytes": capacity.reserve_bytes,
            "resume_hysteresis_bytes": capacity.resume_hysteresis_bytes,
        },
        "index_settings": {"enabled": status.settings.enabled, "max_bytes": status.settings.max_bytes},
        "steps": status.counters.steps, "commits": status.counters.commits,
        "failures": status.counters.failures, "evictions": status.counters.evictions,
        "pauses": status.counters.pauses, "merges": status.counters.merges,
        "in_flight": status.in_flight, "idle_steps": status.idle_steps,
        "forced_steps": status.forced_steps,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use storage::thread_index::{CapacityState, IndexPauseReason, ThreadIndexSettings};
    use storage::{Storage, StorageConfig};

    #[test]
    fn paused_step_and_search_failure_keep_the_exact_writer_admission() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = StorageConfig {
            db_path: dir.path().join("paused.db"),
            thread_index: ThreadIndexSettings {
                enabled: true,
                max_bytes: 512 << 20,
            },
            ..StorageConfig::default()
        };
        cfg.hard_limits.max_wal_bytes = 1;
        let storage = Storage::open(cfg).unwrap();
        let handle = storage.handle();
        let lease = acquired(
            handle
                .acquire_thread_index_lease(&owner(), now(), THREAD_INDEX_LEASE_NS)
                .unwrap(),
        )
        .unwrap();
        let error = step(&handle, &lease).unwrap_err();
        let expected = error_evidence(error.as_ref());
        assert_eq!(
            expected["paused"]["reason"],
            format!(
                "{:?}",
                CapacityState::IndexPaused(IndexPauseReason::WalLimit)
            )
        );
        assert_eq!(expected["paused"]["operation"], "Some(Adjustment)");
        assert_eq!(expected["paused"]["growth"]["wal"], 1_054_752);
        assert_eq!(expected["paused"]["max_wal_bytes"], 1);
        let (done, receive) = mpsc::channel();
        let (commands, _unused) = mpsc::channel();
        done.send(Err(error)).unwrap();
        let mut parallel = Parallel {
            commands,
            receive: Some(receive),
            job: None,
        };
        let error = parallel.search_failed("private body SQL parameters".into());
        let evidence = error_evidence(error.as_ref());
        assert_eq!(evidence["paused"], expected["paused"]);
        assert!(evidence["search_error_sha256"].is_string());
        assert!(!evidence.to_string().contains("private body"));
    }
}
