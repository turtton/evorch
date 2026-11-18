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
//!   at most `batch_rows` rows and at most `max_rows` rows in all. Each
//!   committed batch publishes a progress event.
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
//! * **Owner and disconnect.** An operation remembers the client that started
//!   it (from the connection, never from the request). When that client's last
//!   connection ends the operation is cancelled like a requested cancel; the
//!   operations of other clients, and one started without a known client, run
//!   on (ADR 0045 §10).
//! * **Partial results.** `effect` tells what the committed batches cover: a
//!   failed or cancelled operation that committed batches ends `partial`.
//! * **Unknown outcomes.** The table lives in memory; after a restart it is
//!   empty and a client reads the progress counters of a new operation.
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use storage::StorageHandle;
use storage::thread_index::{IndexOwner, IndexOwnerKind, ReindexRequest, RequestFailure};
use tokio::sync::watch;
use workbench_protocol::errors::{ErrorCode, RetryAdvice, WorkbenchError};
use workbench_protocol::events::UiEvent;
use workbench_protocol::ids::{ClientId, OperationId};
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
use crate::connections::{ConnectionEpoch, ConnectionHooks};
use crate::queries::{Pager, error, revision};

mod reindex;
use reindex::{Advance, Finished, Submitted};

/// Operations the table keeps (running and settled). Beyond it a new id is
/// refused: an id that is forgotten could run a second time.
const MAX_OPERATIONS: usize = 64;

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

/// The test seams of the area; the default is production behaviour.
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct MaintenanceSeams {
    pub hook: Option<MaintenanceHook>,
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
    /// Where the clients' connections are counted, once the area stops work on
    /// a disconnect (see [`MaintenanceArea::stop_on_disconnect`]).
    connections: OnceLock<Arc<ConnectionHooks>>,
}

struct State {
    table: Operations<MaintenanceView>,
    running: HashMap<MaintenanceKind, Running>,
}

/// Who started an operation and during which run of connections of that client.
struct Owner {
    client: ClientId,
    /// The epoch the client was in when the operation was accepted; before
    /// every epoch when it held no connection then.
    epoch: ConnectionEpoch,
}

struct Running {
    id: OperationId,
    /// The client that started it, when the connection said who it was.
    owner: Option<Owner>,
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
    Backfill { batch_rows: usize, max_rows: u64 },
    Reindex { request: ReindexRequest, wait: bool },
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
        /// A writer step failed and may have committed work it did not report.
        unknown: bool,
        /// What the settled view shows besides the failure (a reindex names its
        /// failed result).
        progress: Option<MaintenanceProgress>,
    },
    Cancelled(CancelReason),
}

impl Ended {
    fn storage() -> Self {
        Self::Failed {
            code: ErrorCode::StorageUnavailable,
            message: "the storage is unavailable",
            unknown: false,
            progress: None,
        }
    }

    /// The failure of a writer step: it may have committed part of its work
    /// that it did not report.
    fn unreported(self) -> Self {
        match self {
            Self::Failed { code, progress, .. } => Self::Failed {
                code,
                message: "a batch failed and may have committed part of its work; read the state again",
                unknown: true,
                progress,
            },
            other => other,
        }
    }

    fn internal() -> Self {
        Self::Failed {
            code: ErrorCode::Internal,
            message: "the maintenance task did not finish",
            unknown: false,
            progress: None,
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
            undecodable: 0,
            remaining_unconverted: None,
            remaining_unattributed: None,
        },
        MaintenanceKind::ReindexThreads => MaintenanceProgress::ReindexThreads {
            request_id: None,
            summary: None,
            result: None,
        },
    }
}

impl MaintenanceArea {
    pub(super) fn new(handles: &AreaHandles) -> Result<Self, BootstrapError> {
        let source = handles
            .source("maintenance")
            .map_err(|e| BootstrapError::from(e.message))?;
        let area = Self::from_parts(
            handles.tasks.clone(),
            source,
            handles.storage.clone(),
            MaintenanceSeams::default(),
        );
        area.stop_on_disconnect(&handles.connections);
        Ok(area)
    }

    /// Stop the operations a client started when its last connection ends, and
    /// no one else's (ADR 0045 §10). The hook only records the request; each
    /// operation stops at its next batch boundary on its own task, and a batch
    /// already inside the writer commits first.
    ///
    /// A notice names the epoch of the client's connections that ended
    /// ([`ConnectionEpoch`]) and stops the operations accepted in it or before
    /// it, so a notice that reaches the area late, after the client connected
    /// again and started something, leaves that alone.
    pub fn stop_on_disconnect(&self, connections: &Arc<ConnectionHooks>) {
        let Some(inner) = &self.inner else { return };
        let _ = inner.connections.set(connections.clone());
        let inner = Arc::downgrade(inner);
        connections.register(Arc::new(move |client, epoch| {
            if let Some(inner) = inner.upgrade() {
                inner.stop_operations_of(client, epoch);
            }
        }));
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
                connections: OnceLock::new(),
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
    /// # Errors
    /// `Conflict` for an id used by a different request, `Busy` when an
    /// operation of the task is running, `LimitExceeded` for a new id once the
    /// table holds `MAX_OPERATIONS` operations (finished ones included: none is
    /// forgotten, so a resent id never runs twice), `Draining` once the host
    /// stops.
    pub fn start(
        &self,
        start: &StartMaintenance,
        owner: Option<&ClientId>,
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
                request: ReindexRequest {
                    canonical_project: task.canonical_project.clone(),
                    since_ns: task.since_ns,
                    include_evicted: task.include_evicted,
                },
                wait: task.wait,
            },
        };
        let (view, cancel) = {
            let mut state = lock(&inner.state);
            // Read under the table's lock: an operation that is accepted is
            // never older than the epoch it records.
            let owner = owner.map(|client| Owner {
                client: client.clone(),
                epoch: inner
                    .connections
                    .get()
                    .and_then(|connections| connections.epoch_of(client))
                    .unwrap_or_default(),
            });
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
            // A full table never drops an entry: dropping a finished one would
            // let its id run again when a client resends it.
            if state.table.list().len() >= MAX_OPERATIONS {
                return Err(WorkbenchError::new(
                    ErrorCode::LimitExceeded,
                    "the core holds as many maintenance operations as it can remember; restart the core to start a new one",
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
                    owner,
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
    fn stop_operations_of(&self, client: &ClientId, ended: ConnectionEpoch) {
        for running in lock(&self.state).running.values() {
            let stale = |owner: &Owner| owner.client == *client && owner.epoch <= ended;
            if running.owner.as_ref().is_some_and(stale) {
                running.cancel.send_replace(true);
            }
        }
    }

    async fn at(&self, stage: MaintenanceStage, id: &OperationId, batches: u32) {
        if let Some(hook) = &self.seams.hook {
            hook(stage, id, batches).await;
        }
    }

    async fn run(
        self: Arc<Self>,
        id: OperationId,
        kind: MaintenanceKind,
        work: Work,
        mut stop: StopSignal,
        cancel: watch::Receiver<bool>,
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
            Work::Reindex { request, wait } => {
                self.reindex(&id, request, wait, &mut stop, &cancel).await
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
        let (mut converted, mut reattributed, mut undecodable) = (0_u64, 0_u64, 0_u64);
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
                // A failed step may have committed its conversion before its
                // attribution failed and the writer says nothing of it: the
                // counters keep the reported batches, the effect says unknown.
                Ok(Err(_)) => return Ended::storage().unreported(),
                Err(ended) => return ended.unreported(),
            };
            converted += step.converted;
            reattributed += step.reattributed;
            undecodable += step.undecodable;
            let progress = MaintenanceProgress::BackfillEvents {
                converted,
                reattributed,
                undecodable,
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

    /// A reindex: the request is registered and then driven as the foreground
    /// owner of the index, one pass per loop turn. Every pass is looked at
    /// against a cancel or a stop first; an operation that ends early leaves the
    /// durable request `pending` and gives the lease back.
    async fn reindex(
        &self,
        id: &OperationId,
        request: ReindexRequest,
        wait: bool,
        stop: &mut StopSignal,
        cancel: &watch::Receiver<bool>,
    ) -> Ended {
        if let Some(reason) = Self::interrupted(stop, cancel) {
            return Ended::Cancelled(reason);
        }
        let owner = IndexOwner {
            id: format!("core-{}", id.as_str()),
            kind: IndexOwnerKind::Cli,
        };
        let storage = self.storage.clone();
        let submitted = match blocking(move || {
            reindex::submit(&storage, &request, owner, wait, reindex::clock_ns())
        })
        .await
        {
            Ok(submitted) => submitted,
            Err(ended) => return ended,
        };
        let driver = match submitted {
            Submitted::Failed(reason) => return reindex_failed(None, None, reason),
            Submitted::Delegated { request_id } => {
                return Ended::Completed {
                    outcome: MaintenanceOutcome::Delegated,
                    progress: Some(MaintenanceProgress::ReindexThreads {
                        request_id: Some(request_id.clone()),
                        summary: None,
                        result: Some(reindex::wire_result(
                            &storage::thread_index::ReindexResult::Accepted { request_id },
                        )),
                    }),
                };
            }
            Submitted::Driving(driver) => driver,
        };
        // The request id is known from now on: show it before the first pass.
        self.record(id, 0, reindex::progress(&driver));
        let mut held = Some(driver);
        let mut batches = 0_u32;
        let ended = loop {
            if let Some(reason) = Self::interrupted(stop, cancel) {
                break Ended::Cancelled(reason);
            }
            self.at(MaintenanceStage::BeforeBatch, id, batches).await;
            // The last look before the writer, as for a backfill batch.
            if let Some(reason) = Self::interrupted(stop, cancel) {
                break Ended::Cancelled(reason);
            }
            let Some(mut driver) = held.take() else {
                break Ended::internal();
            };
            let storage = self.storage.clone();
            let passed = blocking(move || {
                let advance = driver.advance(&storage, &mut reindex::clock_ns);
                (driver, advance)
            })
            .await;
            let (driver, advance) = match passed {
                Ok(passed) => passed,
                Err(ended) => break ended,
            };
            let progress = reindex::progress(&driver);
            let request_id = driver.request_id().to_owned();
            held = Some(driver);
            match advance {
                Advance::Done(finished) => {
                    break reindex_ended(&request_id, progress, finished);
                }
                Advance::Progressed => {
                    batches += 1;
                    self.record(id, batches, progress);
                }
                Advance::Wait(pause) => {
                    // Nothing to do until the index wakes (or the owner's
                    // lease expires): wait for that or for a signal.
                    tokio::select! {
                        biased;
                        () = stop.stopped() => {}
                        () = cancelled(cancel.clone()) => {}
                        () = tokio::time::sleep(pause) => {}
                    }
                }
            }
        };
        if let Some(mut driver) = held {
            let storage = self.storage.clone();
            // The lease is fenced and expires by itself: a lost release only
            // delays the next owner.
            let _ = blocking(move || driver.release(&storage)).await;
        }
        ended
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
                    Ended::Failed {
                        code,
                        message,
                        unknown,
                        progress,
                    } => {
                        if let Some(progress) = progress {
                            view.progress = progress.clone();
                        }
                        view.operation.state = OperationState::Failed { code: *code };
                        view.effect = if *unknown {
                            MaintenanceEffect::Unknown
                        } else {
                            partial
                        };
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

async fn cancelled(mut cancel: watch::Receiver<bool>) {
    // A dropped sender is also an end of the operation's owner.
    let _ = cancel.wait_for(|requested| *requested).await;
}

/// How a reindex that submitted nothing, or whose control path broke, fails.
fn reindex_failed(
    request_id: Option<String>,
    summary: Option<workbench_protocol::namespaces::maintenance::ReindexSummary>,
    reason: RequestFailure,
) -> Ended {
    use workbench_protocol::namespaces::maintenance::ReindexResult;
    let wire = reindex::failure(reason);
    let (code, message) = match reason {
        RequestFailure::InvalidArguments => {
            (ErrorCode::InvalidRequest, "the reindex request is invalid")
        }
        RequestFailure::DatabaseUnavailable | RequestFailure::PersistenceUnavailable => {
            (ErrorCode::StorageUnavailable, "the storage is unavailable")
        }
        RequestFailure::UnrecoverableControlFailure => (
            ErrorCode::Internal,
            "the index control failed in a way the reindex cannot recover from",
        ),
    };
    Ended::Failed {
        code,
        message,
        unknown: false,
        progress: Some(MaintenanceProgress::ReindexThreads {
            request_id: request_id.clone(),
            summary,
            result: Some(ReindexResult::Failed {
                request_id,
                reason: wire,
            }),
        }),
    }
}

/// How a reindex the driver finished ends the operation.
fn reindex_ended(request_id: &str, progress: MaintenanceProgress, finished: Finished) -> Ended {
    use storage::thread_index::ReindexResult as Stored;
    let summary = match &progress {
        MaintenanceProgress::ReindexThreads { summary, .. } => summary.clone(),
        MaintenanceProgress::BackfillEvents { .. } => None,
    };
    match finished {
        Finished::Result(Stored::Failed { request_id, reason }) => {
            reindex_failed(request_id, summary, reason)
        }
        Finished::Result(result) => Ended::Completed {
            outcome: match result {
                Stored::Accepted { .. } => MaintenanceOutcome::Delegated,
                _ => MaintenanceOutcome::Completed,
            },
            progress: Some(MaintenanceProgress::ReindexThreads {
                request_id: Some(request_id.to_owned()),
                summary,
                result: Some(reindex::wire_result(&result)),
            }),
        },
        // Another owner cancelled this request; that is not a result of the work.
        Finished::Cancelled => Ended::Failed {
            code: ErrorCode::Conflict,
            message: "the reindex request was cancelled by another owner",
            unknown: false,
            progress: Some(MaintenanceProgress::ReindexThreads {
                request_id: Some(request_id.to_owned()),
                summary,
                result: None,
            }),
        },
    }
}

/// Run `work` on a blocking worker: the storage writer answers synchronously.
/// `Ended` is the one error type of the operation's steps; it carries a reindex's
/// named result, so it is larger than clippy likes and is returned once per step.
#[allow(clippy::result_large_err)]
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
