//! Host service of the `maintenance` area (ADR 0045, stage 17d): the online
//! maintenance of the event database, run by the core through the storage
//! writer it already owns. A client never opens the database.
//!
//! * **One writer.** Every batch is one call on the shared [`StorageHandle`]
//!   (`retention_backfill_step`, the thread index control API). The area opens
//!   no database and builds no writer of its own.
//! * **Handler and task.** `start` only consults the table, inserts the
//!   pending operation and hands the work to a counted task that keeps the
//!   command's restart permit until the operation is settled. The handler never
//!   waits for a batch. Because the task is counted, the host's busy decision
//!   (`AreaTasks::busy`) sees a running maintenance without any extra wiring.
//! * **Bounded batches.** A backfill is a loop of one writer step per batch of
//!   at most `batch_rows` rows and at most `max_rows` rows in all; a reindex is
//!   a loop of one index step per batch. Each committed batch publishes a
//!   progress event.
//! * **Idempotency and exclusion.** The same operation id with the same request
//!   is answered from the table; another request under the id is a `Conflict`.
//!   One operation per task runs at a time: another one is `Busy`, whoever asks.
//! * **Cancel and stop.** Both are looked at before every batch and again right
//!   before it is handed to the writer (so a barrier between the two is
//!   covered). A requested cancel ends the operation `cancelled(requested)`, a
//!   stop of the host `cancelled(shutdown)`; neither commits another batch. A
//!   batch already inside the writer is a transaction and commits first, and
//!   the counters then include it. A task dropped without an outcome settles as
//!   `cancelled(shutdown)`, never as a success.
//! * **Partial results.** `effect` tells what the committed batches cover: a
//!   failed or cancelled operation that committed batches ends `partial`.
//! * **Unknown outcomes.** The table lives in memory; after a restart it is
//!   empty and a client reads the progress counters of a new operation.
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use storage::StorageHandle;
use storage::THREAD_INDEX_LEASE_NS;
use storage::thread_index::{
    IndexLease, IndexOwner, IndexOwnerKind, IndexStepResult, LeaseResult, ReindexRequest,
    ReindexResult, RequestFailure, RequestProgress, RequestSummary,
};
use tokio::sync::watch;
use workbench_protocol::errors::{ErrorCode, RetryAdvice, WorkbenchError};
use workbench_protocol::events::UiEvent;
use workbench_protocol::ids::OperationId;
use workbench_protocol::namespaces::maintenance::{
    DEFAULT_BATCH_ROWS, DEFAULT_MAX_ROWS, MAX_MESSAGE_BYTES, MaintenanceEffect, MaintenanceEvent,
    MaintenanceKind, MaintenanceOutcome, MaintenanceProgress, MaintenanceTask, MaintenanceView,
    StartMaintenance,
};
use workbench_protocol::operation::{CancelReason, OperationState, OperationView};
use workbench_protocol::queries::PageRequest;
use workbench_protocol::views::Page;

use super::settings::operations::{Lookup, Operations, Tracked, digest_of};
use super::{AreaHandles, AreaTasks, ProjectionSource, StopSignal};
use crate::bootstrap::BootstrapError;
use crate::bootstrap::restart::HostCommandPermit;
use crate::queries::{Pager, error, revision};

/// Operations the table keeps (running and settled).
const MAX_OPERATIONS: usize = 64;

/// How long an idle reindex waits before it looks at the index again, at most.
const IDLE_POLL_MAX: Duration = Duration::from_secs(1);
const IDLE_POLL_MIN: Duration = Duration::from_millis(25);

/// Where a test may hold an operation.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceStage {
    /// The task has begun; nothing was done.
    Started,
    /// Everything but the next batch is ready; the next step is the writer.
    /// The `u32` of the hook is the number of batches committed so far.
    BeforeBatch,
}

/// A seam for tests: awaited at each [`MaintenanceStage`] of every operation.
/// Production installs none.
#[doc(hidden)]
pub type MaintenanceHook = Arc<
    dyn Fn(MaintenanceStage, &OperationId, u32) -> Pin<Box<dyn Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// A seam for tests: how an idle reindex waits before it looks again. Production
/// sleeps (and wakes at once for a cancel or a stop).
#[doc(hidden)]
pub type MaintenancePause =
    Arc<dyn Fn(Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// A seam for tests: the wall clock of the index control calls, in Unix
/// nanoseconds.
#[doc(hidden)]
pub type MaintenanceClock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The test seams of the area; the default is production behaviour.
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct MaintenanceSeams {
    pub hook: Option<MaintenanceHook>,
    pub pause: Option<MaintenancePause>,
    pub clock: Option<MaintenanceClock>,
}

/// An accepted request.
pub struct Accepted {
    pub view: MaintenanceView,
    /// The request was already accepted under this id: nothing new runs.
    pub already_applied: bool,
}

/// The `maintenance` service, reached from `ingress/namespaces/maintenance.rs`
/// as `self.areas.maintenance`. Without handles (the unwired form) every request
/// is refused.
#[derive(Default)]
pub struct MaintenanceArea {
    inner: Option<Arc<Inner>>,
    pager: Pager,
}

struct Inner {
    state: Mutex<State>,
    tasks: AreaTasks,
    source: ProjectionSource,
    storage: StorageHandle,
    seams: MaintenanceSeams,
}

struct State {
    table: Operations<MaintenanceView>,
    running: HashMap<MaintenanceKind, Running>,
}

struct Running {
    id: OperationId,
    cancel: watch::Sender<bool>,
}

impl Tracked for MaintenanceView {
    fn operation(&self) -> &OperationView {
        &self.operation
    }

    fn operation_mut(&mut self) -> &mut OperationView {
        &mut self.operation
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a started operation does, resolved by the handler.
enum Work {
    Backfill {
        batch_rows: usize,
        max_rows: u64,
    },
    Reindex {
        /// The directory of the project to reindex, when one is named.
        project_root: Option<PathBuf>,
        since_ns: Option<i64>,
        include_evicted: bool,
    },
}

/// How an operation ended.
enum Ended {
    Completed {
        outcome: MaintenanceOutcome,
        progress: Option<MaintenanceProgress>,
    },
    Failed {
        code: ErrorCode,
        message: &'static str,
    },
    Cancelled(CancelReason),
}

impl Ended {
    fn storage() -> Self {
        Self::Failed {
            code: ErrorCode::StorageUnavailable,
            message: "the storage is unavailable",
        }
    }

    fn internal() -> Self {
        Self::Failed {
            code: ErrorCode::Internal,
            message: "the maintenance task did not finish",
        }
    }
}

fn bounded(text: &str) -> String {
    let mut end = text.len().min(MAX_MESSAGE_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn initial_progress(kind: MaintenanceKind) -> MaintenanceProgress {
    match kind {
        MaintenanceKind::BackfillEvents => MaintenanceProgress::BackfillEvents {
            converted: 0,
            reattributed: 0,
            remaining_unconverted: None,
            remaining_unattributed: None,
        },
        MaintenanceKind::ReindexThreads => MaintenanceProgress::ReindexThreads {
            request_id: None,
            target_count: 0,
            succeeded: 0,
            unindexed: 0,
        },
    }
}

fn reindex_progress(request_id: &str, summary: &RequestSummary) -> MaintenanceProgress {
    let unindexed = summary.gaps.iter().map(|gap| gap.count).sum();
    MaintenanceProgress::ReindexThreads {
        request_id: Some(request_id.chars().take(128).collect()),
        target_count: summary.target_count,
        succeeded: summary.succeeded,
        unindexed,
    }
}

impl MaintenanceArea {
    pub(super) fn new(handles: &AreaHandles) -> Result<Self, BootstrapError> {
        let source = handles
            .source("maintenance")
            .map_err(|e| BootstrapError::from(e.message))?;
        Ok(Self::from_parts(
            handles.tasks.clone(),
            source,
            handles.storage.clone(),
            MaintenanceSeams::default(),
        ))
    }

    /// The service over its parts: the host's tasks, the namespace's event
    /// source and the storage writer (the only way it writes).
    pub fn from_parts(
        tasks: AreaTasks,
        source: ProjectionSource,
        storage: StorageHandle,
        seams: MaintenanceSeams,
    ) -> Self {
        Self {
            inner: Some(Arc::new(Inner {
                state: Mutex::new(State {
                    table: Operations::new(MAX_OPERATIONS),
                    running: HashMap::new(),
                }),
                tasks,
                source,
                storage,
                seams,
            })),
            pager: Pager::default(),
        }
    }

    fn inner(&self) -> Result<&Arc<Inner>, WorkbenchError> {
        self.inner.as_ref().ok_or_else(|| {
            error(
                ErrorCode::UnsupportedCapability,
                "maintenance is not available in this host",
            )
        })
    }

    /// Accept a maintenance operation: start it, or answer from the table.
    ///
    /// `project_root` is the directory of the project a reindex names.
    ///
    /// # Errors
    /// `Conflict` for an id used by a different request, `Busy` when an
    /// operation of the task is running, `LimitExceeded` when every retained
    /// operation is still running, `Draining` once the host stops.
    pub fn start(
        &self,
        start: &StartMaintenance,
        project_root: Option<PathBuf>,
        accepted: HostCommandPermit,
    ) -> Result<Accepted, WorkbenchError> {
        let inner = self.inner()?;
        let kind = start.task.kind();
        let digest = digest_of(&[&serde_json::to_vec(&start.task)
            .map_err(|_| error(ErrorCode::InvalidRequest, "the task cannot be encoded"))?]);
        let work = match &start.task {
            MaintenanceTask::BackfillEvents(task) => Work::Backfill {
                batch_rows: task.batch_rows.unwrap_or(DEFAULT_BATCH_ROWS) as usize,
                max_rows: task.max_rows.unwrap_or(DEFAULT_MAX_ROWS),
            },
            MaintenanceTask::ReindexThreads(task) => Work::Reindex {
                project_root,
                since_ns: task.since_ns,
                include_evicted: task.include_evicted,
            },
        };
        let (view, cancel) = {
            let mut state = lock(&inner.state);
            match state.table.lookup(&start.operation_id, &digest) {
                Lookup::Same(view) => {
                    return Ok(Accepted {
                        view,
                        already_applied: true,
                    });
                }
                Lookup::Conflict => {
                    return Err(WorkbenchError::new(
                        ErrorCode::Conflict,
                        "this operation id was used for a different request",
                        RetryAdvice::Never,
                    ));
                }
                Lookup::New => {}
            }
            if state.running.contains_key(&kind) {
                return Err(WorkbenchError::new(
                    ErrorCode::Busy,
                    "another operation of this task is running; list the maintenance operations to see it",
                    RetryAdvice::UserActionRequired,
                ));
            }
            let revision = state.table.next_revision();
            let view = MaintenanceView {
                operation: OperationView {
                    operation_id: start.operation_id.clone(),
                    state: OperationState::Pending,
                    revision,
                    result: None,
                },
                task: kind,
                batches: 0,
                progress: initial_progress(kind),
                effect: MaintenanceEffect::None,
                outcome: None,
                message: None,
            };
            state.table.insert(view.clone(), digest)?;
            let (cancel, observed) = watch::channel(false);
            state.running.insert(
                kind,
                Running {
                    id: start.operation_id.clone(),
                    cancel,
                },
            );
            (view, observed)
        };
        let this = inner.clone();
        let id = start.operation_id.clone();
        let spawned =
            inner
                .tasks
                .spawn_counted_tracked("maintenance", Some(accepted), move |stop| {
                    this.run(id, kind, work, stop, cancel)
                });
        if let Err(refused) = spawned {
            let mut state = lock(&inner.state);
            state.table.forget(&start.operation_id);
            state.running.remove(&kind);
            return Err(WorkbenchError::new(
                ErrorCode::Draining,
                refused.to_string(),
                RetryAdvice::AfterReconnect,
            ));
        }
        Ok(Accepted {
            view,
            already_applied: false,
        })
    }

    /// Ask a running operation to stop at its next batch boundary; the final
    /// view of one that already ended. The operation settles by itself.
    ///
    /// # Errors
    /// `NotFound` for an operation the core does not hold.
    pub fn cancel(&self, id: &OperationId) -> Result<Accepted, WorkbenchError> {
        let inner = self.inner()?;
        let state = lock(&inner.state);
        let view = state.table.get(id).cloned().ok_or_else(unknown)?;
        let already_applied = match state.running.get(&view.task) {
            Some(running) if running.id == *id => running.cancel.send_replace(true),
            _ => true,
        };
        Ok(Accepted {
            view,
            already_applied,
        })
    }

    /// One operation, running or finished.
    ///
    /// # Errors
    /// `NotFound` for an operation the core does not hold.
    pub fn get(&self, id: &OperationId) -> Result<MaintenanceView, WorkbenchError> {
        let inner = self.inner()?;
        lock(&inner.state)
            .table
            .get(id)
            .cloned()
            .ok_or_else(unknown)
    }

    /// The operations the core holds, newest first.
    ///
    /// # Errors
    /// `Conflict` for a cursor that is not this list's.
    pub fn list(&self, request: &PageRequest) -> Result<Page<MaintenanceView>, WorkbenchError> {
        let items = lock(&self.inner()?.state).table.list();
        self.pager
            .page("maintenance:list", &items, request, revision(&items))
    }
}

fn unknown() -> WorkbenchError {
    error(
        ErrorCode::NotFound,
        "unknown maintenance operation; if it was started before a restart, start a new one and read its counters",
    )
}

impl Inner {
    async fn at(&self, stage: MaintenanceStage, id: &OperationId, batches: u32) {
        if let Some(hook) = &self.seams.hook {
            hook(stage, id, batches).await;
        }
    }

    fn now_ns(&self) -> i64 {
        match &self.seams.clock {
            Some(clock) => clock(),
            None => storage::system_time_to_ns(std::time::SystemTime::now()).unwrap_or(i64::MAX),
        }
    }

    async fn run(
        self: Arc<Self>,
        id: OperationId,
        kind: MaintenanceKind,
        work: Work,
        mut stop: StopSignal,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<(), BootstrapError> {
        let mut guard = Settle {
            inner: self.clone(),
            id: id.clone(),
            kind,
            settled: false,
        };
        self.at(MaintenanceStage::Started, &id, 0).await;
        let ended = match work {
            Work::Backfill {
                batch_rows,
                max_rows,
            } => {
                self.backfill(&id, batch_rows, max_rows, &stop, &cancel)
                    .await
            }
            Work::Reindex {
                project_root,
                since_ns,
                include_evicted,
            } => {
                self.reindex(
                    &id,
                    project_root,
                    since_ns,
                    include_evicted,
                    &mut stop,
                    &mut cancel,
                )
                .await
            }
        };
        guard.finish(&ended);
        Ok(())
    }

    /// The reason the operation must not commit another batch, if any.
    fn interrupted(stop: &StopSignal, cancel: &watch::Receiver<bool>) -> Option<CancelReason> {
        if stop.is_stopped() {
            Some(CancelReason::Shutdown)
        } else if *cancel.borrow() {
            Some(CancelReason::Requested)
        } else {
            None
        }
    }

    async fn backfill(
        &self,
        id: &OperationId,
        batch_rows: usize,
        max_rows: u64,
        stop: &StopSignal,
        cancel: &watch::Receiver<bool>,
    ) -> Ended {
        let mut batches = 0_u32;
        let (mut converted, mut reattributed) = (0_u64, 0_u64);
        loop {
            if let Some(reason) = Self::interrupted(stop, cancel) {
                return Ended::Cancelled(reason);
            }
            let handled = converted + reattributed;
            if handled >= max_rows {
                // The bound of this operation: the counters tell what is left.
                return Ended::Completed {
                    outcome: MaintenanceOutcome::Completed,
                    progress: None,
                };
            }
            let rows = batch_rows.min(usize::try_from(max_rows - handled).unwrap_or(usize::MAX));
            self.at(MaintenanceStage::BeforeBatch, id, batches).await;
            // The last look before the writer: a stop or a cancel that came
            // while the task was held means the batch must not be committed.
            if let Some(reason) = Self::interrupted(stop, cancel) {
                return Ended::Cancelled(reason);
            }
            let storage = self.storage.clone();
            let step = match blocking(move || storage.retention_backfill_step(rows)).await {
                Ok(Ok(step)) => step,
                Ok(Err(_)) => return Ended::storage(),
                Err(ended) => return ended,
            };
            converted += step.converted;
            reattributed += step.reattributed;
            let progress = MaintenanceProgress::BackfillEvents {
                converted,
                reattributed,
                remaining_unconverted: Some(step.remaining_unconverted),
                remaining_unattributed: Some(step.remaining_unattributed),
            };
            if step.converted + step.reattributed == 0 {
                return Ended::Completed {
                    outcome: MaintenanceOutcome::Completed,
                    progress: Some(progress),
                };
            }
            batches += 1;
            self.record(id, batches, progress.clone());
            if step.remaining_unconverted == 0 && step.remaining_unattributed == 0 {
                return Ended::Completed {
                    outcome: MaintenanceOutcome::Completed,
                    progress: Some(progress),
                };
            }
        }
    }

    async fn reindex(
        &self,
        id: &OperationId,
        project_root: Option<PathBuf>,
        since_ns: Option<i64>,
        include_evicted: bool,
        stop: &mut StopSignal,
        cancel: &mut watch::Receiver<bool>,
    ) -> Ended {
        let canonical_project = match project_root {
            None => None,
            Some(root) => match blocking(move || {
                root.canonicalize()
                    .ok()
                    .filter(|path| path.is_dir())
                    .and_then(|path| path.into_os_string().into_string().ok())
            })
            .await
            {
                Ok(Some(path)) => Some(path),
                Ok(None) => {
                    return Ended::Failed {
                        code: ErrorCode::NotFound,
                        message: "the project directory is not available",
                    };
                }
                Err(ended) => return ended,
            },
        };
        let request = ReindexRequest {
            canonical_project,
            since_ns,
            include_evicted,
        };
        let owner = IndexOwner {
            id: format!("core-{}-{id}", std::process::id()),
            kind: IndexOwnerKind::Cli,
        };
        let now = self.now_ns();
        let storage = self.storage.clone();
        let submitted = {
            let owner = owner.clone();
            blocking(move || {
                storage.submit_thread_index_request(&request, &owner, now, THREAD_INDEX_LEASE_NS)
            })
            .await
        };
        let submission = match submitted {
            Ok(Ok(submission)) => submission,
            Ok(Err(reason)) => return failure_of(reason),
            Err(ended) => return ended,
        };
        let request_id = submission.request_id;
        let mut lease = match submission.lease {
            LeaseResult::Acquired(lease) => Some(lease),
            // Another live owner carries the durable request on.
            LeaseResult::HeldByOther(_) | LeaseResult::Lost
                if matches!(submission.progress, RequestProgress::Pending(_)) =>
            {
                return Ended::Completed {
                    outcome: MaintenanceOutcome::Delegated,
                    progress: Some(MaintenanceProgress::ReindexThreads {
                        request_id: Some(request_id.chars().take(128).collect()),
                        target_count: 0,
                        succeeded: 0,
                        unindexed: 0,
                    }),
                };
            }
            _ => None,
        };
        let ended = self
            .drive(
                id,
                &owner,
                &request_id,
                submission.progress,
                &mut lease,
                stop,
                cancel,
            )
            .await;
        if let Some(lease) = lease {
            // Fenced: a lost lease cannot release its successor.
            let storage = self.storage.clone();
            let _ = blocking(move || storage.release_thread_index_lease(&lease)).await;
        }
        ended
    }

    #[allow(clippy::too_many_arguments)]
    async fn drive(
        &self,
        id: &OperationId,
        owner: &IndexOwner,
        request_id: &str,
        mut progress: RequestProgress,
        lease: &mut Option<IndexLease>,
        stop: &mut StopSignal,
        cancel: &mut watch::Receiver<bool>,
    ) -> Ended {
        let mut batches = 0_u32;
        let mut backoff = IDLE_POLL_MIN;
        loop {
            match progress {
                RequestProgress::Terminal(result) => return settled_reindex(request_id, result),
                RequestProgress::Cancelled => return Ended::Cancelled(CancelReason::Requested),
                RequestProgress::Pending(_) => {}
            }
            if let Some(reason) = Self::interrupted(stop, cancel) {
                return self.cancel_request(request_id, reason).await;
            }
            self.at(MaintenanceStage::BeforeBatch, id, batches).await;
            if let Some(reason) = Self::interrupted(stop, cancel) {
                return self.cancel_request(request_id, reason).await;
            }
            let now = self.now_ns();
            let mut next_due = None;
            let storage = self.storage.clone();
            let owner = owner.clone();
            let held = lease.take();
            let step = blocking(move || {
                let mut lease = match held {
                    Some(current)
                        if current.expires_at_ns.saturating_sub(now)
                            <= THREAD_INDEX_LEASE_NS / 2 =>
                    {
                        match storage.heartbeat_thread_index_lease(
                            &current,
                            now,
                            THREAD_INDEX_LEASE_NS,
                        )? {
                            LeaseResult::Acquired(renewed) => Some(renewed),
                            _ => None,
                        }
                    }
                    Some(current) => Some(current),
                    None => {
                        match storage.acquire_thread_index_lease(
                            &owner,
                            now,
                            THREAD_INDEX_LEASE_NS,
                        )? {
                            LeaseResult::Acquired(acquired) => Some(acquired),
                            LeaseResult::HeldByOther(other) => {
                                return Ok((None, None, Some(other.expires_at_ns)));
                            }
                            LeaseResult::Lost => None,
                        }
                    }
                };
                let mut stepped = None;
                if let Some(current) = lease.as_ref() {
                    stepped = Some(storage.step_thread_index(current, now)?);
                    if matches!(stepped, Some(IndexStepResult::LeaseLost)) {
                        lease = None;
                    }
                }
                Ok::<_, RequestFailure>((lease, stepped, None))
            })
            .await;
            let mut made_progress = false;
            match step {
                Ok(Ok((renewed, stepped, due))) => {
                    *lease = renewed;
                    next_due = due.or(next_due);
                    match stepped {
                        Some(IndexStepResult::Progress) => made_progress = true,
                        Some(IndexStepResult::Idle { next_wake_at_ns }) => {
                            next_due = next_wake_at_ns;
                        }
                        _ => {}
                    }
                }
                Ok(Err(reason)) => return failure_of(reason),
                Err(ended) => return ended,
            }
            let storage = self.storage.clone();
            let asked = request_id.to_owned();
            let at = self.now_ns();
            let status =
                match blocking(move || storage.thread_index_request_status(&asked, at)).await {
                    Ok(Ok(status)) => status,
                    Ok(Err(reason)) => return failure_of(reason),
                    Err(ended) => return ended,
                };
            progress = status.progress;
            if made_progress {
                batches += 1;
                backoff = IDLE_POLL_MIN;
                if let RequestProgress::Pending(summary) = &progress {
                    self.record(id, batches, reindex_progress(request_id, summary));
                }
            } else if matches!(progress, RequestProgress::Pending(_)) {
                let now = self.now_ns();
                let until_due = next_due
                    .map(|due| Duration::from_nanos(u64::try_from(due - now).unwrap_or(0)))
                    .unwrap_or(backoff);
                self.pause(
                    backoff.min(until_due).max(Duration::from_millis(1)),
                    stop,
                    cancel,
                )
                .await;
                backoff = (backoff * 2).min(IDLE_POLL_MAX);
            }
        }
    }

    /// Wait before the index is looked at again; a stop or a cancel ends it.
    async fn pause(
        &self,
        wait: Duration,
        stop: &mut StopSignal,
        cancel: &mut watch::Receiver<bool>,
    ) {
        let waited: Pin<Box<dyn Future<Output = ()> + Send>> = match &self.seams.pause {
            Some(pause) => pause(wait),
            None => Box::pin(tokio::time::sleep(wait)),
        };
        tokio::select! {
            () = waited => {}
            () = stop.stopped() => {}
            _ = cancel.wait_for(|requested| *requested) => {}
        }
    }

    /// End a reindex that is stopped: a requested cancel also cancels the
    /// durable request (shared work for other requests continues); a stop of
    /// the host leaves the request for the next owner of the index.
    async fn cancel_request(&self, request_id: &str, reason: CancelReason) -> Ended {
        if reason == CancelReason::Requested {
            let storage = self.storage.clone();
            let asked = request_id.to_owned();
            let at = self.now_ns();
            let _ = blocking(move || storage.cancel_thread_index_request(&asked, at)).await;
        }
        Ended::Cancelled(reason)
    }

    /// A committed batch: keep its counters and tell the clients.
    fn record(&self, id: &OperationId, batches: u32, progress: MaintenanceProgress) {
        let view = lock(&self.state).table.update(id, |view| {
            view.batches = batches;
            view.progress = progress;
            if batches > 0 {
                view.effect = MaintenanceEffect::Partial;
            }
        });
        if let Some(view) = view {
            self.publish(MaintenanceEvent::MaintenanceProgress {
                view: Box::new(view),
            });
        }
    }

    fn publish(&self, event: MaintenanceEvent) {
        let _ = self.source.publish(UiEvent::Maintenance(event));
    }

    fn settle(&self, id: &OperationId, kind: MaintenanceKind, ended: &Ended) {
        let view = {
            let mut state = lock(&self.state);
            let view = state.table.update(id, |view| {
                let committed = view.batches > 0;
                let partial = if committed {
                    MaintenanceEffect::Partial
                } else {
                    MaintenanceEffect::None
                };
                match ended {
                    Ended::Completed { outcome, progress } => {
                        view.operation.state = OperationState::Ready;
                        view.effect = MaintenanceEffect::Complete;
                        view.outcome = Some(*outcome);
                        if let Some(progress) = progress {
                            view.progress = progress.clone();
                        }
                    }
                    Ended::Failed { code, message } => {
                        view.operation.state = OperationState::Failed { code: *code };
                        view.effect = partial;
                        view.message = Some(bounded(message));
                    }
                    Ended::Cancelled(reason) => {
                        view.operation.state = OperationState::Cancelled { reason: *reason };
                        view.effect = partial;
                        view.message = Some(bounded(match reason {
                            CancelReason::Requested => {
                                "cancelled at a batch boundary; the committed batches stay"
                            }
                            _ => {
                                "the core stopped; the committed batches stay, start a new operation to continue"
                            }
                        }));
                    }
                }
            });
            if state.running.get(&kind).is_some_and(|run| run.id == *id) {
                state.running.remove(&kind);
            }
            view
        };
        if let Some(view) = view {
            self.publish(MaintenanceEvent::MaintenanceSettled {
                view: Box::new(view),
            });
        }
    }
}

fn failure_of(reason: RequestFailure) -> Ended {
    match reason {
        RequestFailure::InvalidArguments => Ended::Failed {
            code: ErrorCode::InvalidRequest,
            message: "the index refused the request",
        },
        RequestFailure::DatabaseUnavailable | RequestFailure::PersistenceUnavailable => {
            Ended::storage()
        }
        RequestFailure::UnrecoverableControlFailure => Ended::Failed {
            code: ErrorCode::Internal,
            message: "the index could not run",
        },
    }
}

fn settled_reindex(request_id: &str, result: ReindexResult) -> Ended {
    match result {
        ReindexResult::Completed { summary, .. } => Ended::Completed {
            outcome: MaintenanceOutcome::Completed,
            progress: Some(reindex_progress(request_id, &summary)),
        },
        ReindexResult::CompletedWithGaps { summary, .. } => Ended::Completed {
            outcome: MaintenanceOutcome::CompletedWithGaps,
            progress: Some(reindex_progress(request_id, &summary)),
        },
        ReindexResult::Accepted { .. } => Ended::Completed {
            outcome: MaintenanceOutcome::Delegated,
            progress: None,
        },
        ReindexResult::Failed { reason, .. } => failure_of(reason),
    }
}

/// Run `work` on a blocking worker: the storage writer answers synchronously.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Ended> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| Ended::internal())
}

/// Settles an operation as cancelled when its task is dropped without an outcome.
struct Settle {
    inner: Arc<Inner>,
    id: OperationId,
    kind: MaintenanceKind,
    settled: bool,
}

impl Settle {
    fn finish(&mut self, ended: &Ended) {
        self.settled = true;
        self.inner.settle(&self.id, self.kind, ended);
    }
}

impl Drop for Settle {
    fn drop(&mut self) {
        if !self.settled {
            self.inner.settle(
                &self.id,
                self.kind,
                &Ended::Cancelled(CancelReason::Shutdown),
            );
        }
    }
}
