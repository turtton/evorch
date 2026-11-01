//! Background reclaim of confirmed families while the process runs.
//!
//! One process per database does this work: the one holding `<db>.reclaim-lock`.
//! Others never call `reclaim_step`. The executor deletes database rows in bounded
//! steps (through the storage writer, which is the only writer), then the files of
//! families whose rows are gone. It never touches owner rows or scratch directories:
//! those belong to the exclusive startup phase, because a second process may share
//! the ownership root while this one runs.
//!
//! A family's files are `done` only after a pass over the artifact metadata that
//! examined all of it without a single failure. A budget stop continues from a
//! cursor, a listing or file failure makes the round `files_failed` and is retried,
//! and a family that keeps failing is reported (never skipped silently and never
//! marked done).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use storage::thread_retention::{
    DEFAULT_RETENTION_BACKFILL_ROWS, FilesOutcome, ReclaimBudget, ReclaimJobMembers,
    ReclaimJobState,
};
use storage::{Database, StorageConfig, StorageHandle};

use crate::artifacts::{ArtifactStore, MetaStop};

/// A family whose files failed this many times stays `files_failed` (reported).
pub const FILES_RETRY_LIMIT: u32 = 20;
/// Meta files examined per cycle across all families.
pub const META_SCANS_PER_CYCLE: usize = 5_000;
/// Jobs listed per state and cycle.
pub const JOBS_PER_PAGE: usize = 16;
/// Exhausted families named in a report; the rest are counted.
const EXHAUSTED_LISTED: usize = 64;
/// Failure details kept per family and round.
const FAILURES_KEPT: usize = 8;

const FILE_STATES: [ReclaimJobState; 3] = [
    ReclaimJobState::DbDone,
    ReclaimJobState::FilesPending,
    ReclaimJobState::FilesFailed,
];

pub struct ExecutorConfig {
    pub handle: StorageHandle,
    /// Locates the database for the executor's own read connection and the lease.
    pub storage: StorageConfig,
    /// Without a store the executor cannot finish any family's files and leaves them
    /// as they are (another process with a store does).
    pub artifacts: Option<Arc<ArtifactStore>>,
    /// Pause between cycles.
    pub interval: Duration,
    /// Cycles between attempts to take the reclaim lease while another process holds it.
    pub lease_retry_cycles: u32,
    pub files_retry_limit: u32,
    pub jobs_per_page: usize,
    pub meta_scans_per_cycle: usize,
}

impl ExecutorConfig {
    #[must_use]
    pub fn new(
        handle: StorageHandle,
        storage: StorageConfig,
        artifacts: Option<Arc<ArtifactStore>>,
    ) -> Self {
        Self {
            handle,
            storage,
            artifacts,
            interval: Duration::from_secs(5),
            lease_retry_cycles: 12,
            files_retry_limit: FILES_RETRY_LIMIT,
            jobs_per_page: JOBS_PER_PAGE,
            meta_scans_per_cycle: META_SCANS_PER_CYCLE,
        }
    }
}

/// What the latest cycle saw. `errors` are that cycle's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutorReport {
    pub cycles: u64,
    /// This process holds the reclaim lease and works.
    pub holds_lease: bool,
    pub backfill_remaining: u64,
    pub rows_reclaimed: u64,
    pub jobs_db_pending: u64,
    /// Jobs only the exclusive startup phase can finish (reported, not worked on).
    pub jobs_waiting_exclusive: u64,
    pub oversized_pending: u64,
    pub files_done: u64,
    pub files_failed: u64,
    /// Families whose files still fail after the retry limit (the first few).
    pub files_exhausted: Vec<String>,
    /// How many families are exhausted, listed or not.
    pub files_exhausted_total: usize,
    pub errors: Vec<String>,
    /// The thread ended (shutdown or failure to start).
    pub finished: bool,
}

#[derive(Default)]
struct FamilyFiles {
    after: Option<String>,
    failures: Vec<String>,
    failed_total: usize,
    attempts: u32,
}

/// One executor's state. [`Self::run_once`] is a whole cycle and is what the thread
/// repeats; tests drive it directly.
pub struct ReclaimWorker {
    config: ExecutorConfig,
    reader: Option<Database>,
    lease: Option<File>,
    cycles: u64,
    backfill_done: bool,
    files: HashMap<String, FamilyFiles>,
    exhausted: BTreeSet<String>,
    /// Where the next page of each file-state listing starts.
    cursors: [Option<String>; 3],
}

impl ReclaimWorker {
    #[must_use]
    pub fn new(config: ExecutorConfig) -> Self {
        Self {
            config,
            reader: None,
            lease: None,
            cycles: 0,
            backfill_done: false,
            files: HashMap::new(),
            exhausted: BTreeSet::new(),
            cursors: [None, None, None],
        }
    }

    fn reader(&mut self) -> Result<&Database, String> {
        if self.reader.is_none() {
            self.reader =
                Some(Database::open(&self.config.storage).map_err(|error| error.to_string())?);
        }
        Ok(self.reader.as_ref().expect("opened above"))
    }

    fn ensure_lease(&mut self, report: &mut ExecutorReport) {
        if self.lease.is_some() {
            return;
        }
        let retry = u64::from(self.config.lease_retry_cycles.max(1));
        if !self.cycles.is_multiple_of(retry) {
            return;
        }
        match storage::reclaim_lease(&self.config.storage.db_path) {
            Ok(lease) => self.lease = lease,
            Err(error) => report.errors.push(format!("reclaim lease: {error}")),
        }
    }

    /// One cycle: backfill step, database reclaim step, then files of families
    /// whose rows are gone. Does nothing (and writes nothing) without the lease.
    pub fn run_once(&mut self) -> ExecutorReport {
        let mut report = ExecutorReport::default();
        self.ensure_lease(&mut report);
        self.cycles += 1;
        report.cycles = self.cycles;
        report.holds_lease = self.lease.is_some();
        if report.holds_lease {
            self.backfill(&mut report);
            self.reclaim(&mut report);
            if self.config.artifacts.is_some() {
                self.remove_files(&mut report);
            }
        }
        report.files_exhausted_total = self.exhausted.len();
        report.files_exhausted = self
            .exhausted
            .iter()
            .take(EXHAUSTED_LISTED)
            .cloned()
            .collect();
        report
    }

    fn backfill(&mut self, report: &mut ExecutorReport) {
        if self.backfill_done {
            return;
        }
        match self
            .config
            .handle
            .retention_backfill_step(DEFAULT_RETENTION_BACKFILL_ROWS)
        {
            Ok(step) => {
                report.backfill_remaining =
                    step.remaining_unconverted + step.remaining_unattributed;
                // New events are written already attributed, so once nothing is left
                // nothing can become left.
                self.backfill_done = report.backfill_remaining == 0;
            }
            Err(error) => report.errors.push(format!("backfill: {error}")),
        }
    }

    fn reclaim(&mut self, report: &mut ExecutorReport) {
        match self.config.handle.reclaim_step(ReclaimBudget::default()) {
            Ok(step) => {
                report.rows_reclaimed = step.rows;
                report.jobs_db_pending = step.jobs_db_pending;
                report.jobs_waiting_exclusive = step.jobs_waiting_exclusive;
                report.oversized_pending = step.oversized_pending;
                if let Some(error) = step.last_error {
                    report.errors.push(format!("reclaim: {error}"));
                }
            }
            Err(error) => report.errors.push(format!("reclaim: {error}")),
        }
    }

    /// The next page of `state`'s jobs. Pages follow the family key, so a family that
    /// is skipped (exhausted) or slow never keeps later families from being reached;
    /// after the last page the listing starts over.
    fn next_page(&mut self, slot: usize) -> Result<Vec<ReclaimJobMembers>, String> {
        let limit = self.config.jobs_per_page.max(1);
        let after = self.cursors[slot].clone();
        let page = self
            .reader()?
            .reclaim_jobs_after(FILE_STATES[slot], after.as_deref(), limit)
            .map_err(|error| error.to_string())?;
        self.cursors[slot] = if page.len() < limit {
            None
        } else {
            page.last().map(|job| job.family_key.clone())
        };
        Ok(page)
    }

    fn remove_files(&mut self, report: &mut ExecutorReport) {
        let mut scans_left = self.config.meta_scans_per_cycle;
        for slot in 0..FILE_STATES.len() {
            let page = match self.next_page(slot) {
                Ok(page) => page,
                Err(error) => {
                    report.errors.push(format!("reclaim jobs: {error}"));
                    // A broken reader is reopened next cycle.
                    self.reader = None;
                    return;
                }
            };
            for job in page {
                if self.files.get(&job.family_key).map_or(0, |f| f.attempts)
                    >= self.config.files_retry_limit
                {
                    self.exhausted.insert(job.family_key);
                    continue;
                }
                if scans_left == 0 {
                    return;
                }
                self.remove_family_files(&job, &mut scans_left, report);
            }
        }
    }

    fn remove_family_files(
        &mut self,
        job: &ReclaimJobMembers,
        scans_left: &mut usize,
        report: &mut ExecutorReport,
    ) {
        let Some(artifacts) = self.config.artifacts.clone() else {
            return;
        };
        let threads = job.threads.iter().cloned().collect::<HashSet<_>>();
        let runs = job.runs.iter().cloned().collect::<HashSet<_>>();
        let entry = self.files.entry(job.family_key.clone()).or_default();
        let removal =
            artifacts.remove_meta_after(&threads, &runs, entry.after.as_deref(), *scans_left);
        *scans_left = scans_left.saturating_sub(removal.scanned.max(1));
        entry.failed_total += removal.failed_total;
        for (path, error) in &removal.failed {
            if entry.failures.len() < FAILURES_KEPT {
                entry.failures.push(format!("{}: {error}", path.display()));
            }
        }
        if removal.stop == Some(MetaStop::Budget) {
            // More to examine: continue next cycle, and say the files are underway.
            entry.after = removal.last_name;
            self.mark(&job.family_key, FilesOutcome::Pending, report);
            return;
        }
        // Complete, or the listing failed (which counted as a failure above): either
        // way this round is over and the next one starts from the beginning.
        entry.after = None;
        if entry.failed_total == 0 {
            self.files.remove(&job.family_key);
            if self.mark(&job.family_key, FilesOutcome::Done, report) {
                report.files_done += 1;
            }
        } else {
            entry.attempts += 1;
            report.errors.push(format!(
                "files of {}: {} failure(s), first: {}",
                job.family_key,
                entry.failed_total,
                entry.failures.first().map_or("(listing)", String::as_str)
            ));
            entry.failures.clear();
            entry.failed_total = 0;
            if self.mark(&job.family_key, FilesOutcome::Failed, report) {
                report.files_failed += 1;
            }
        }
    }

    fn mark(&self, family: &str, outcome: FilesOutcome, report: &mut ExecutorReport) -> bool {
        match self.config.handle.reclaim_mark_files(family, outcome) {
            Ok(changed) => changed,
            Err(error) => {
                report
                    .errors
                    .push(format!("mark files of {family}: {error}"));
                false
            }
        }
    }
}

struct Shared {
    report: Mutex<ExecutorReport>,
    changed: Condvar,
}

/// The reclaim thread. [`Self::shutdown`] joins it: call it before dropping the
/// storage it uses.
pub struct ReclaimExecutor {
    shared: Arc<Shared>,
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ReclaimExecutor {
    /// Start the thread; the first cycle runs at once.
    ///
    /// # Errors
    /// Fails when the thread cannot be spawned.
    pub fn spawn(config: ExecutorConfig) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            report: Mutex::new(ExecutorReport::default()),
            changed: Condvar::new(),
        });
        let (stop, stopped) = mpsc::channel::<()>();
        let interval = config.interval;
        let mut worker = ReclaimWorker::new(config);
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("thread-retention-reclaim".into())
            .spawn(move || {
                loop {
                    let report = worker.run_once();
                    for error in &report.errors {
                        tracing::warn!(%error, "thread retention reclaim");
                    }
                    publish(&thread_shared, report, false);
                    // The only event is shutdown; a timeout starts the next cycle.
                    if !matches!(
                        stopped.recv_timeout(interval),
                        Err(RecvTimeoutError::Timeout)
                    ) {
                        break;
                    }
                }
                let mut last = lock(&thread_shared.report).clone();
                last.finished = true;
                publish(&thread_shared, last, true);
            })?;
        Ok(Self {
            shared,
            stop,
            thread: Some(thread),
        })
    }

    /// The latest cycle's report.
    #[must_use]
    pub fn report(&self) -> ExecutorReport {
        lock(&self.shared.report).clone()
    }

    /// Block until `cycles` cycles ran (or the thread ended) and return the report.
    #[must_use]
    pub fn wait_for_cycles(&self, cycles: u64) -> ExecutorReport {
        let mut report = lock(&self.shared.report);
        while report.cycles < cycles && !report.finished {
            report = self
                .shared
                .changed
                .wait(report)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        report.clone()
    }

    /// Stop and join. The reclaim lease is released with the thread.
    pub fn shutdown(mut self) {
        self.join();
    }

    fn join(&mut self) {
        // A closed channel means the thread already ended.
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::error!("the thread retention reclaim thread panicked");
        }
    }
}

impl Drop for ReclaimExecutor {
    fn drop(&mut self) {
        self.join();
    }
}

fn lock(report: &Mutex<ExecutorReport>) -> std::sync::MutexGuard<'_, ExecutorReport> {
    report
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn publish(shared: &Shared, report: ExecutorReport, finished: bool) {
    let mut slot = lock(&shared.report);
    *slot = ExecutorReport { finished, ..report };
    shared.changed.notify_all();
}
