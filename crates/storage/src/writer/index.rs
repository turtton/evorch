//! Phase B conversation index job (ADR 0041 順序 5, W2).
//!
//! One owner per database drives the index through the shared step: the GUI's
//! writer runs it in the background (`start_thread_index_owner`), a CLI with no
//! live owner runs the same step in the foreground. Idle time goes first, but
//! after 32 ordinary commands or 100 ms the writer always takes one step.
//! Extraction runs on one extractor thread outside the writer; the commit
//! re-checks lease epoch, revision, generation, state, versions and request.
//! Nothing wakes the writer while there is no work, the index is disabled or
//! paused, or work waits for a retry time.

use std::path::Path;
use std::sync::mpsc::{self, Sender, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};
use secret_guard::SecretRedactor;

use crate::thread_index::{
    CapacityState, IndexCapacity, IndexCapacityStatus, IndexLease, IndexOwner, IndexPauseReason,
    IndexSettingsReceipt, IndexStepResult, LeaseResult, PlannedGrowth, ReindexRequest,
    RequestFailure, RequestGapReason, RequestStatus, RequestSubmission, ThreadIndexControl,
    ThreadIndexSettings,
};
use crate::{StorageConfig, StorageError};

mod capacity;
#[cfg(test)]
mod gate_measure;
mod lease;
mod requests;
#[cfg(test)]
mod tests;
mod wait;
mod work;

pub use work::ThreadIndexCounters;
pub(crate) use work::{Env, IndexRuntime};

/// Lease length of a long-lived (GUI) owner and how often it is renewed.
pub const THREAD_INDEX_LEASE_NS: i64 = 30_000_000_000;
const HEARTBEAT: Duration = Duration::from_secs(10);
/// A step is forced after this many ordinary commands or this much time.
const STEP_EVERY_COMMANDS: u32 = 32;
const STEP_EVERY: Duration = Duration::from_millis(100);

fn now_ns() -> i64 {
    crate::system_time_to_ns(std::time::SystemTime::now()).unwrap_or(i64::MAX)
}

/// The shared GUI/CLI control boundary over the writer connection.
pub(crate) struct Control<'a> {
    pub env: Env<'a>,
    pub rt: &'a mut IndexRuntime,
    pub redactor: &'a SecretRedactor,
    pub now_ns: i64,
}

fn persistence(error: StorageError) -> RequestFailure {
    tracing::warn!(error = %error, "thread index request persistence failed");
    RequestFailure::PersistenceUnavailable
}

fn unavailable(error: StorageError) -> RequestFailure {
    tracing::warn!(error = %error, "thread index database unavailable");
    RequestFailure::DatabaseUnavailable
}

impl ThreadIndexControl for Control<'_> {
    fn submit_request(
        &mut self,
        request: &ReindexRequest,
        owner: &IndexOwner,
        now_ns: i64,
        lease_duration_ns: i64,
    ) -> Result<RequestSubmission, RequestFailure> {
        requests::validate(request)?;
        let conn = self.env.conn;
        let rt = &mut *self.rt;
        let mut wal_pending = false;
        let (request_id, lease) = (|| {
            let transaction = rusqlite::Transaction::new_unchecked(
                conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            work::discover_all(&transaction, rt, now_ns)?;
            let (request_id, _) = requests::submit_in(&transaction, request, now_ns)?;
            let lease = lease::acquire_in(&transaction, owner, now_ns, lease_duration_ns)?;
            // A parked owner does not run another step for each request.
            // Finalize policy gaps at acceptance instead of waiting forever.
            let status = work::capacity_status(
                self.env,
                rt,
                PlannedGrowth::Bounded {
                    db: 0,
                    wal: 0,
                    shm: 0,
                },
            )?;
            // A WAL-only refusal is the bounded wait, not a policy gap: the
            // request stays pending and expires on its own deadline. A WAL
            // pause left by an earlier expiry is released so the next step
            // admits (or waits) again from this request's own start.
            wal_pending = status.state == CapacityState::IndexPaused(IndexPauseReason::WalLimit)
                && rt.pause.is_none_or(|pause| pause.state == status.state);
            if !wal_pending
                && status.state != CapacityState::Normal
                && status.state != CapacityState::IndexPaused(IndexPauseReason::SettingsAdjustment)
            {
                let gap = if !rt.settings.enabled {
                    RequestGapReason::IndexDisabled
                } else {
                    RequestGapReason::CapacityPaused
                };
                requests::gap_pending(&transaction, gap)?;
                requests::finalize(&transaction, now_ns)?;
            }
            transaction.commit()?;
            Ok::<_, StorageError>((request_id, lease))
        })()
        .map_err(persistence)?;
        if wal_pending {
            let rt = &mut *self.rt;
            rt.pause = None;
            rt.paused_growth = None;
            rt.paused_work = None;
            if let Some(wait) = rt.wal_wait.as_mut() {
                wait.advance_to(self.now_ns);
            }
        }
        let status = self.request_status(&request_id, now_ns)?;
        Ok(RequestSubmission {
            request_id,
            lease,
            progress: status.progress,
        })
    }

    fn request_status(
        &self,
        request_id: &str,
        now_ns: i64,
    ) -> Result<RequestStatus, RequestFailure> {
        requests::status(self.env.conn, request_id, now_ns)
            .map_err(unavailable)?
            .ok_or(RequestFailure::InvalidArguments)
    }

    fn cancel_request(&mut self, request_id: &str) -> Result<RequestStatus, RequestFailure> {
        requests::cancel(self.env.conn, request_id, self.now_ns).map_err(persistence)?;
        self.request_status(request_id, self.now_ns)
    }

    fn acquire_lease(
        &mut self,
        owner: &IndexOwner,
        now_ns: i64,
        lease_duration_ns: i64,
    ) -> Result<LeaseResult, RequestFailure> {
        lease::acquire(self.env.conn, owner, now_ns, lease_duration_ns).map_err(persistence)
    }

    fn heartbeat(
        &mut self,
        lease: &IndexLease,
        now_ns: i64,
        lease_duration_ns: i64,
    ) -> Result<LeaseResult, RequestFailure> {
        lease::heartbeat(self.env.conn, lease, now_ns, lease_duration_ns).map_err(persistence)
    }

    fn release_lease(&mut self, lease: &IndexLease) -> Result<(), RequestFailure> {
        lease::release(self.env.conn, lease).map_err(persistence)
    }

    fn step(&mut self, lease: &IndexLease, now_ns: i64) -> Result<IndexStepResult, RequestFailure> {
        // Only in-process fixtures use inline extraction. Production callers
        // enter IndexCommand::Step and the same extractor as the GUI owner.
        #[cfg(test)]
        {
            work::foreground_step(self.env, self.rt, lease, now_ns, self.redactor).map_err(
                |error| {
                    tracing::warn!(error = %error, "thread index step failed");
                    let _ = requests::fail_pending(self.env.conn, lease, now_ns);
                    RequestFailure::UnrecoverableControlFailure
                },
            )
        }
        #[cfg(not(test))]
        {
            let _ = (lease, now_ns, self.redactor);
            unreachable!("production steps must use the shared extractor")
        }
    }
}

impl IndexCapacity for Control<'_> {
    fn capacity_status(
        &mut self,
        growth: PlannedGrowth,
    ) -> Result<IndexCapacityStatus, StorageError> {
        work::capacity_status(self.env, self.rt, growth)
    }

    fn apply_index_settings(
        &mut self,
        settings: ThreadIndexSettings,
    ) -> Result<IndexSettingsReceipt, StorageError> {
        work::apply_settings(self.env, self.rt, settings, self.now_ns)
    }
}

/// The job is waiting for a pinned WAL to be checkpointed instead of pausing
/// (ADR 0041 R5). No index content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalWaitStatus {
    /// When the wait began (`thread_index_state.wal_wait_since_ns`).
    pub since_ns: i64,
    /// The WAL-only admission refusal that started the wait.
    pub refused: IndexCapacityStatus,
    /// When the next non-blocking checkpoint probe is due.
    pub next_retry_at_ns: i64,
    /// Probes run since the wait began.
    pub probes: u32,
}

/// Diagnostics of the background job (no index content).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadIndexJobStatus {
    pub settings: ThreadIndexSettings,
    pub settings_generation: u64,
    pub owner_lease: Option<IndexLease>,
    pub paused: Option<IndexCapacityStatus>,
    /// Waiting for the WAL to recover before pausing; `None` while not waiting.
    pub wal_wait: Option<WalWaitStatus>,
    pub in_flight: bool,
    /// The writer will wake for index work without a new command.
    pub wake_scheduled: bool,
    pub retry_wake_in: Option<Duration>,
    pub idle_steps: u64,
    pub forced_steps: u64,
    pub counters: ThreadIndexCounters,
    #[cfg(test)]
    pub step_timings_us: Vec<(&'static str, u128)>,
    #[cfg(test)]
    pub commit_timings_us: Vec<(&'static str, u128)>,
}

pub(super) type ReplySender<T, E> = Sender<Result<T, E>>;
type Reply<T> = ReplySender<T, StorageError>;
type ControlReply<T> = ReplySender<T, RequestFailure>;

pub(super) enum IndexCommand {
    StartOwner(IndexOwner, SyncSender<super::Command>, Reply<LeaseResult>),
    StopOwner(Reply<()>),
    Submit(
        Box<ReindexRequest>,
        IndexOwner,
        i64,
        i64,
        ControlReply<RequestSubmission>,
    ),
    Status(String, i64, ControlReply<RequestStatus>),
    Cancel(String, i64, ControlReply<RequestStatus>),
    Acquire(IndexOwner, i64, i64, ControlReply<LeaseResult>),
    Heartbeat(IndexLease, i64, i64, ControlReply<LeaseResult>),
    Release(IndexLease, ControlReply<()>),
    Step(
        IndexLease,
        i64,
        SyncSender<super::Command>,
        ControlReply<IndexStepResult>,
    ),
    Capacity(Reply<IndexCapacityStatus>),
    ApplySettings(ThreadIndexSettings, Reply<IndexSettingsReceipt>),
    JobStatus(Reply<ThreadIndexJobStatus>),
    Extracted(Box<work::Extracted>),
    #[cfg(test)]
    Block(mpsc::Receiver<()>),
    /// Test observer: receives the counters after every background step.
    #[cfg(test)]
    Observe(Sender<ThreadIndexCounters>),
    /// Freeze the state machine's clock independently of real scheduler time.
    #[cfg(test)]
    Clock(std::sync::Arc<std::sync::atomic::AtomicI64>),
    /// Make a periodic owner tick due without sleeping in tests.
    #[cfg(test)]
    Tick,
}

struct OwnerState {
    owner: IndexOwner,
    lease: Option<IndexLease>,
    next_heartbeat: Instant,
    #[cfg(test)]
    next_heartbeat_ns: i64,
    sender: SyncSender<super::Command>,
}

struct Extractor {
    jobs: Sender<work::Work>,
    _handle: JoinHandle<()>,
}

/// Background owner state kept by the writer thread.
pub(super) struct IndexJob {
    rt: IndexRuntime,
    redactor: SecretRedactor,
    owner: Option<OwnerState>,
    extractor: Option<Extractor>,
    in_flight: bool,
    maybe_work: bool,
    retry_wake: Option<Instant>,
    commands_since_step: u32,
    last_step: Instant,
    idle_steps: u64,
    forced_steps: u64,
    #[cfg(test)]
    observer: Option<Sender<ThreadIndexCounters>>,
    #[cfg(test)]
    step_timings_us: Vec<(&'static str, u128)>,
    #[cfg(test)]
    clock: Option<std::sync::Arc<std::sync::atomic::AtomicI64>>,
}

/// Whether a ready index step runs now: idle first, but never starved.
pub(super) fn should_step(idle: bool, commands_since_step: u32, since_last_step: Duration) -> bool {
    idle || commands_since_step >= STEP_EVERY_COMMANDS || since_last_step >= STEP_EVERY
}

fn env<'a>(conn: &'a Connection, config: &'a StorageConfig, source_suspended: bool) -> Env<'a> {
    Env {
        conn,
        db_path: &config.db_path,
        limits: &config.hard_limits,
        source_suspended,
    }
}

impl IndexJob {
    /// Applies the startup settings before any discovery so new rows get the
    /// current extractor/accounting versions.
    pub(super) fn new(conn: &Connection, config: &StorageConfig, source_suspended: bool) -> Self {
        let mut rt = IndexRuntime::new(config.thread_index);
        let env = env(conn, config, source_suspended);
        if let Err(error) = work::configure_fts(conn).and_then(|()| {
            work::apply_settings(env, &mut rt, config.thread_index, now_ns()).map(drop)
        }) {
            tracing::warn!(error = %error, "thread index settings could not be applied");
        }
        Self {
            rt,
            redactor: SecretRedactor::from_env(),
            owner: None,
            extractor: None,
            in_flight: false,
            maybe_work: true,
            retry_wake: None,
            commands_since_step: 0,
            last_step: Instant::now(),
            idle_steps: 0,
            forced_steps: 0,
            #[cfg(test)]
            observer: None,
            #[cfg(test)]
            step_timings_us: Vec::new(),
            #[cfg(test)]
            clock: None,
        }
    }

    /// A job that never owns the lease (writer unit-test fixtures).
    #[cfg(test)]
    pub(super) fn inert() -> Self {
        Self {
            rt: IndexRuntime::new(ThreadIndexSettings {
                enabled: false,
                max_bytes: crate::thread_index::DEFAULT_THREAD_INDEX_MAX_BYTES,
            }),
            redactor: SecretRedactor::from_env(),
            owner: None,
            extractor: None,
            in_flight: false,
            maybe_work: false,
            retry_wake: None,
            commands_since_step: 0,
            last_step: Instant::now(),
            idle_steps: 0,
            forced_steps: 0,
            #[cfg(test)]
            observer: None,
            #[cfg(test)]
            step_timings_us: Vec::new(),
            #[cfg(test)]
            clock: None,
        }
    }

    fn lease(&self) -> Option<&IndexLease> {
        self.owner.as_ref().and_then(|owner| owner.lease.as_ref())
    }

    fn now_ns(&self) -> i64 {
        #[cfg(test)]
        if let Some(clock) = &self.clock {
            return clock.load(std::sync::atomic::Ordering::SeqCst);
        }
        now_ns()
    }

    fn wants_step(&self) -> bool {
        self.lease().is_some()
            && self.rt.settings.enabled
            && self.rt.pause.is_none()
            && !self.in_flight
            && !(self.rt.wal_wait.is_some() && self.retry_wake.is_some())
            && (self.rt.ready.is_some()
                || self.rt.merge_pending
                || !self.rt.discovery_complete
                || self.maybe_work)
    }

    /// The next instant the writer must wake for the index without commands.
    pub(super) fn deadline(&self, now: Instant) -> Option<Instant> {
        let owner = self.owner.as_ref()?;
        let mut deadline = owner.next_heartbeat;
        if self.wants_step() {
            deadline = deadline.min(now);
        } else if let Some(retry) = self.retry_wake.filter(|_| self.lease().is_some()) {
            deadline = deadline.min(retry);
        }
        Some(deadline)
    }

    /// A due WAL wait must not sit behind its backoff timer: the next step
    /// runs the probe.
    fn wake_due_wait(&mut self, now_ns: i64) {
        if self.rt.wal_wait.is_some_and(|wait| wait.probe_due(now_ns)) {
            self.retry_wake = None;
            self.maybe_work = true;
        }
    }

    /// Called once per handled ordinary command.
    pub(super) fn note_command(&mut self) {
        self.commands_since_step = self.commands_since_step.saturating_add(1);
        self.maybe_work = true;
    }

    fn status(&self, now: Instant) -> ThreadIndexJobStatus {
        ThreadIndexJobStatus {
            settings: self.rt.settings,
            settings_generation: self.rt.settings_generation,
            owner_lease: self.lease().cloned(),
            paused: self.rt.pause,
            wal_wait: self.rt.wal_wait.map(|wait| WalWaitStatus {
                since_ns: wait.since_ns,
                refused: wait.refused,
                next_retry_at_ns: wait.next_retry_at_ns,
                probes: wait.probes,
            }),
            in_flight: self.in_flight,
            wake_scheduled: self.wants_step(),
            retry_wake_in: self
                .retry_wake
                .map(|wake| wake.saturating_duration_since(now)),
            idle_steps: self.idle_steps,
            forced_steps: self.forced_steps,
            counters: self.rt.counters,
            #[cfg(test)]
            step_timings_us: self.step_timings_us.clone(),
            #[cfg(test)]
            commit_timings_us: self.rt.commit_timings_us.clone(),
        }
    }

    fn heartbeat(&mut self, conn: &Connection, now: Instant) {
        let now_ns = self.now_ns();
        #[cfg(test)]
        let clock_frozen = self.clock.is_some();
        let Some(owner) = self.owner.as_mut() else {
            return;
        };
        #[cfg(test)]
        if clock_frozen && now_ns < owner.next_heartbeat_ns {
            owner.next_heartbeat = now + HEARTBEAT;
            return;
        }
        if now < owner.next_heartbeat {
            return;
        }
        owner.next_heartbeat = now + HEARTBEAT;
        #[cfg(test)]
        {
            owner.next_heartbeat_ns = now_ns.saturating_add(HEARTBEAT.as_nanos() as i64);
        }
        let result = match owner.lease.as_ref() {
            Some(lease) => lease::heartbeat(conn, lease, now_ns, THREAD_INDEX_LEASE_NS),
            None => lease::acquire(conn, &owner.owner, now_ns, THREAD_INDEX_LEASE_NS),
        };
        owner.lease = match result {
            Ok(LeaseResult::Acquired(lease)) => Some(lease),
            Ok(LeaseResult::HeldByOther(_) | LeaseResult::Lost) => None,
            Err(error) => {
                tracing::warn!(error = %error, "thread index lease renewal failed");
                None
            }
        };
        if owner.lease.is_some() {
            self.maybe_work = true;
            match work::resume_pending_wal_request(conn, &mut self.rt) {
                Ok(true) => self.retry_wake = None,
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(error = %error, "thread index pending request recheck failed")
                }
            }
        }
    }

    fn dispatch(
        &mut self,
        config: &StorageConfig,
        work: work::Work,
        sender: SyncSender<super::Command>,
    ) {
        if self.extractor.is_none() {
            self.extractor = spawn_extractor(&config.db_path, sender);
        }
        match self.extractor.as_ref() {
            Some(extractor) => match extractor.jobs.send(work) {
                Ok(()) => self.in_flight = true,
                Err(error) => {
                    self.rt.ready = Some((error.0, Err(work::ExtractFailure::Transient)));
                    self.extractor = None;
                }
            },
            None => {
                tracing::warn!("thread index extractor unavailable");
                self.rt.ready = Some((work, Err(work::ExtractFailure::Transient)));
            }
        }
    }
}

fn spawn_extractor(db_path: &Path, sender: SyncSender<super::Command>) -> Option<Extractor> {
    let (jobs, rx) = mpsc::channel::<work::Work>();
    let path = db_path.to_owned();
    let handle = std::thread::Builder::new()
        .name("thread-index-extract".into())
        .spawn(move || {
            let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
            let conn = Connection::open_with_flags(&path, flags).and_then(|conn| {
                conn.busy_timeout(Duration::from_secs(5))?;
                Ok(conn)
            });
            let redactor = SecretRedactor::from_env();
            for work in rx {
                let result = match &conn {
                    Ok(conn) => work::extract(conn, &work, &redactor),
                    Err(_) => Err(work::ExtractFailure::Transient),
                };
                let message =
                    super::Command::Index(IndexCommand::Extracted(Box::new((work, result))));
                if sender.send(message).is_err() {
                    break;
                }
            }
        })
        .map_err(|error| tracing::warn!(error = %error, "thread index extractor spawn failed"))
        .ok()?;
    Some(Extractor {
        jobs,
        _handle: handle,
    })
}

/// Runs at most one index step when one is due.
pub(super) fn run_due(
    job: &mut IndexJob,
    conn: &Connection,
    config: &StorageConfig,
    source_suspended: bool,
    idle: bool,
) {
    let now = Instant::now();
    job.heartbeat(conn, now);
    if job.retry_wake.is_some_and(|wake| now >= wake) {
        job.retry_wake = None;
        job.maybe_work = true;
    }
    // Source commands can invalidate a parked result without changing the
    // physical sizes. Release only that stale pause before deciding to sleep.
    if job.rt.paused_work.is_some() {
        match work::discard_stale_pause(env(conn, config, source_suspended), &mut job.rt) {
            Ok(true) => job.maybe_work = true,
            Ok(false) => {}
            Err(error) => tracing::warn!(error = %error, "thread index paused work recheck failed"),
        }
    }
    if !job.wants_step()
        || !should_step(
            idle,
            job.commands_since_step,
            now.duration_since(job.last_step),
        )
    {
        return;
    }
    if idle {
        job.idle_steps += 1;
    } else {
        job.forced_steps += 1;
    }
    job.commands_since_step = 0;
    job.last_step = now;
    let Some(lease) = job.lease().cloned() else {
        return;
    };
    let env = env(conn, config, source_suspended);
    let now_ns = job.now_ns();
    #[cfg(test)]
    let before = (Instant::now(), job.rt.counters, job.rt.discovery_complete);
    match work::prepare(env, &mut job.rt, &lease, now_ns) {
        Ok(work::Action::Extract(work)) => {
            if let Some(owner) = job.owner.as_ref() {
                job.dispatch(config, work, owner.sender.clone());
            }
        }
        Ok(work::Action::Idle { next_wake_at_ns }) => {
            job.maybe_work = false;
            job.retry_wake = next_wake_at_ns.map(|at| {
                let delay = u64::try_from(at.saturating_sub(now_ns)).unwrap_or(0);
                now + Duration::from_nanos(delay)
            });
        }
        Ok(work::Action::LeaseLost) => {
            if let Some(owner) = job.owner.as_mut() {
                owner.lease = None;
                owner.next_heartbeat = now + HEARTBEAT;
            }
        }
        Ok(work::Action::Paused(_) | work::Action::Progress) => {}
        Err(error) => {
            tracing::warn!(error = %error, "thread index step failed");
            if let Err(error) = requests::fail_pending(conn, &lease, now_ns) {
                tracing::warn!(error = %error, "thread index failure could not be persisted");
            }
            job.maybe_work = false;
        }
    }
    #[cfg(test)]
    if std::env::var_os("EVORCH_INDEX_GATE_WORK").is_some() {
        let kind = if job.rt.counters.commits > before.1.commits {
            "commit"
        } else if job.rt.counters.merges > before.1.merges {
            "merge"
        } else if job.rt.counters.evictions > before.1.evictions {
            "evict"
        } else if !before.2 {
            "discovery"
        } else {
            "bookkeeping"
        };
        job.step_timings_us
            .push((kind, before.0.elapsed().as_micros()));
    }
    notify_observer(job);
}

#[cfg(test)]
fn notify_observer(job: &IndexJob) {
    if let Some(observer) = &job.observer {
        let _ = observer.send(job.rt.counters);
    }
}

#[cfg(not(test))]
fn notify_observer(_job: &IndexJob) {}

/// Capacity may have changed (maintenance checkpoint, cleanup): re-judge a
/// pause once; nothing is retried on a short timer.
pub(super) fn on_capacity_change(
    job: &mut IndexJob,
    conn: &Connection,
    config: &StorageConfig,
    source_suspended: bool,
) {
    let now_ns = job.now_ns();
    if let Some(wait) = job.rt.wal_wait.as_mut() {
        // A reader may have left: probe now instead of after the backoff.
        wait.advance_to(now_ns);
        job.retry_wake = None;
        job.maybe_work = true;
        // The notification itself is the retry event: step in this iteration
        // instead of waiting for the idle timer.
        job.commands_since_step = STEP_EVERY_COMMANDS;
    }
    if job.rt.pause.is_none() {
        return;
    }
    let env = env(conn, config, source_suspended);
    match work::recheck_pause(env, &mut job.rt) {
        Ok(true) => job.maybe_work = true,
        Ok(false) => {}
        Err(error) => tracing::warn!(error = %error, "thread index capacity recheck failed"),
    }
}

pub(super) fn handle(
    job: &mut IndexJob,
    conn: &Connection,
    config: &StorageConfig,
    source_suspended: bool,
    command: IndexCommand,
) {
    let env = env(conn, config, source_suspended);
    let now = job.now_ns();
    let mut control = Control {
        env,
        rt: &mut job.rt,
        redactor: &job.redactor,
        now_ns: now,
    };
    match command {
        IndexCommand::StartOwner(owner, sender, reply) => {
            let result = lease::acquire(conn, &owner, now, THREAD_INDEX_LEASE_NS);
            let lease = match &result {
                Ok(LeaseResult::Acquired(lease)) => Some(lease.clone()),
                _ => None,
            };
            job.owner = Some(OwnerState {
                owner,
                lease,
                next_heartbeat: Instant::now() + HEARTBEAT,
                #[cfg(test)]
                next_heartbeat_ns: now.saturating_add(HEARTBEAT.as_nanos() as i64),
                sender,
            });
            job.maybe_work = true;
            let _ = reply.send(result);
        }
        IndexCommand::StopOwner(reply) => {
            let _ = reply.send(stop_owner(job, conn));
        }
        IndexCommand::Submit(request, owner, now_ns, duration, reply) => {
            let _ = reply.send(control.submit_request(&request, &owner, now_ns, duration));
            job.maybe_work = true;
            job.wake_due_wait(now);
        }
        IndexCommand::Status(request_id, now_ns, reply) => {
            let _ = reply.send(control.request_status(&request_id, now_ns));
        }
        IndexCommand::Cancel(request_id, now_ns, reply) => {
            control.now_ns = now_ns;
            let _ = reply.send(control.cancel_request(&request_id));
        }
        IndexCommand::Acquire(owner, now_ns, duration, reply) => {
            let _ = reply.send(control.acquire_lease(&owner, now_ns, duration));
        }
        IndexCommand::Heartbeat(lease, now_ns, duration, reply) => {
            let _ = reply.send(control.heartbeat(&lease, now_ns, duration));
        }
        IndexCommand::Release(lease, reply) => {
            let _ = reply.send(control.release_lease(&lease));
        }
        IndexCommand::Step(lease, now_ns, sender, reply) => {
            // No snapshot read or extraction may occupy the writer, including
            // a CLI's foreground loop. Only one extraction can be outstanding.
            let result = (|| {
                if !lease::is_valid(conn, &lease, now_ns)? {
                    return Ok(IndexStepResult::LeaseLost);
                }
                if job.in_flight {
                    return Ok(IndexStepResult::Idle {
                        next_wake_at_ns: Some(now_ns.saturating_add(50_000_000)),
                    });
                }
                let action = work::prepare(env, &mut job.rt, &lease, now_ns)?;
                Ok(match action {
                    work::Action::Extract(work) => {
                        job.dispatch(config, work, sender);
                        IndexStepResult::Progress
                    }
                    work::Action::Progress => IndexStepResult::Progress,
                    work::Action::Idle { next_wake_at_ns } => {
                        IndexStepResult::Idle { next_wake_at_ns }
                    }
                    work::Action::Paused(status) => IndexStepResult::Paused(status),
                    work::Action::LeaseLost => IndexStepResult::LeaseLost,
                })
            })()
            .map_err(|error: StorageError| {
                tracing::warn!(error = %error, "thread index step failed");
                if let Err(error) = requests::fail_pending(conn, &lease, now_ns) {
                    tracing::warn!(error = %error, "thread index failure could not be persisted");
                }
                RequestFailure::UnrecoverableControlFailure
            });
            let _ = reply.send(result);
        }
        IndexCommand::Capacity(reply) => {
            let _ = reply.send(control.capacity_status(PlannedGrowth::Bounded {
                db: 0,
                wal: 0,
                shm: 0,
            }));
        }
        IndexCommand::ApplySettings(settings, reply) => {
            let _ = reply.send(control.apply_index_settings(settings));
            job.maybe_work = true;
            job.wake_due_wait(now);
        }
        IndexCommand::JobStatus(reply) => {
            let _ = reply.send(Ok(job.status(Instant::now())));
        }
        IndexCommand::Extracted(extracted) => {
            job.in_flight = false;
            job.rt.ready = Some(*extracted);
            notify_observer(job);
        }
        #[cfg(test)]
        IndexCommand::Block(release) => {
            let _ = release.recv();
        }
        #[cfg(test)]
        IndexCommand::Observe(observer) => job.observer = Some(observer),
        #[cfg(test)]
        IndexCommand::Clock(clock) => {
            job.rt.admission_clock = Some(clock.clone());
            job.clock = Some(clock);
        }
        #[cfg(test)]
        IndexCommand::Tick => {
            if let Some(owner) = job.owner.as_mut() {
                owner.next_heartbeat = Instant::now();
                owner.next_heartbeat_ns = now;
            }
            job.retry_wake = None;
            job.maybe_work = true;
            job.commands_since_step = STEP_EVERY_COMMANDS;
        }
    }
}

fn stop_owner(job: &mut IndexJob, conn: &Connection) -> Result<(), StorageError> {
    job.rt.ready = None;
    // Only the start of the wait is durable; the next owner rebuilds the rest.
    job.rt.wal_wait = None;
    // Keep the single worker across owner restarts. Joining here can deadlock:
    // its completion sender may be waiting on this writer's full queue.
    match job.owner.take().and_then(|owner| owner.lease) {
        Some(lease) => lease::release(conn, &lease),
        None => Ok(()),
    }
}

/// Releases the lease on writer shutdown so the next owner need not wait.
pub(super) fn shutdown(job: &mut IndexJob, conn: &Connection) {
    if let Err(error) = stop_owner(job, conn) {
        tracing::warn!(error = %error, "thread index lease release failed");
    }
    // Closing the jobs channel ends the worker after its outstanding result.
    // Dropping its join handle never waits on a full completion queue.
    job.extractor = None;
}
