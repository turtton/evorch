//! The machinery the `memory` and `improvement` updates share (PR 14): the
//! bounded table of update operations and the task that runs one.
//!
//! * **One writer.** An update is one call of the storage's conditional update
//!   (`update_memory`, `update_improvement`), quoting the revision the client
//!   read; check and write are one transaction on the storage writer. Nothing
//!   here reads a state and decides from it. The only read is the project the
//!   entity belongs to (an immutable fact), done by the caller's `prepare` step.
//! * **Handler and task.** The handler only looks at the table, inserts the
//!   pending operation and hands the work to a counted task, which holds the
//!   command's restart permit until the outcome is settled. The task never
//!   waits for a person.
//! * **Idempotency.** The same operation id with the same request is answered
//!   from the table (nothing runs twice); the same id with another request is a
//!   `Conflict`.
//! * **No stale commit.** The task looks at the host's stop signal before it
//!   starts and again immediately before the write; a stop in between settles
//!   the operation as `Cancelled` and writes nothing. An operation whose task
//!   was dropped without an outcome is settled as `Cancelled` too, never as a
//!   success.
//! * **Unknown outcomes.** The table lives in memory. After a restart it is
//!   empty, so a client that asks for the update gets `not_found` and must read
//!   the entity's state instead of resending.
use crate::areas::settings::operations::{Lookup, Operations, Tracked, digest_of};
use crate::areas::{AreaTasks, StopSignal};
use crate::bootstrap::BootstrapError;
use crate::bootstrap::restart::HostCommandPermit;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use workbench_protocol::{
    errors::{ErrorCode, RetryAdvice, WorkbenchError},
    ids::OperationId,
    management::DenialReason,
    operation::{CancelReason, OperationState, OperationView},
};

/// Updates one area keeps (running and settled).
const MAX_UPDATES: usize = 256;

/// Longest message of an update view.
const MAX_MESSAGE_BYTES: usize = workbench_protocol::namespaces::memory::MAX_UPDATE_MESSAGE_BYTES;

/// What an update that was written (or already was in place) reports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Done {
    /// The entity's revision after the update.
    pub revision: u64,
    /// Whether anything was written.
    pub changed: bool,
    /// The draft a triage attached, by file name.
    pub draft_file: Option<String>,
}

/// Why an update did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub code: ErrorCode,
    pub denial: Option<DenialReason>,
    pub message: String,
}

impl Failure {
    pub fn new(code: ErrorCode, message: &str) -> Self {
        Self {
            code,
            denial: None,
            message: bounded(message),
        }
    }

    /// The entity is not in this project, or not at all (the two are not told
    /// apart).
    pub fn not_found(what: &str) -> Self {
        Self::new(ErrorCode::NotFound, what)
    }

    pub fn stale(what: &str) -> Self {
        Self::new(
            ErrorCode::StaleRevision,
            &format!("the {what} changed since it was read; read it again and decide again"),
        )
    }

    pub fn denied(reason: DenialReason) -> Self {
        Self {
            code: match reason {
                DenialReason::AlreadyResolved => ErrorCode::AlreadyResolved,
                _ => ErrorCode::Conflict,
            },
            denial: Some(reason),
            message: "the current state does not allow this action".into(),
        }
    }

    /// A storage failure. Its text can carry secret-bearing paths, so it is
    /// neither logged nor shown.
    pub fn storage() -> Self {
        Self::new(ErrorCode::StorageUnavailable, "the storage is unavailable")
    }
}

/// How an update ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    Done(Done),
    Failed(Failure),
    Cancelled(CancelReason),
}

/// A view of an update: the shared part and how an outcome is written into it.
pub(in crate::areas) trait UpdateView: Tracked {
    fn settle(&mut self, outcome: &Settled);
}

/// Write the outcome into the shared part of a view; the part every update
/// view shares.
pub(in crate::areas) fn settle_operation(view: &mut OperationView, outcome: &Settled) {
    view.state = match outcome {
        Settled::Done(_) => OperationState::Ready,
        Settled::Failed(failure) => OperationState::Failed { code: failure.code },
        Settled::Cancelled(reason) => OperationState::Cancelled { reason: *reason },
    };
}

/// The sentence of a cancelled update.
pub(in crate::areas) fn cancelled_message() -> String {
    bounded(
        "the core stopped before the outcome was known; read the current state instead of resending",
    )
}

pub(in crate::areas) fn bounded(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    let mut end = line.len().min(MAX_MESSAGE_BYTES);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line[..end].to_owned()
}

/// Where a test may hold an update.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateStage {
    /// The task has begun; nothing was read or written.
    Started,
    /// Everything but the conditional update is done; the next step is the write.
    BeforeCommit,
}

/// A seam for tests: awaited at each [`UpdateStage`] of every update. Production
/// installs none.
#[doc(hidden)]
pub type UpdateHook = Arc<
    dyn Fn(UpdateStage, &OperationId) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync,
>;

/// The write step of an update, produced by its prepare step.
pub(in crate::areas) type Commit = Box<dyn FnOnce() -> Result<Done, Failure> + Send>;

/// The read-only step: check what is immutable (the project of the entity) and
/// get ready, without writing.
pub(in crate::areas) type Prepare = Box<dyn FnOnce() -> Result<Commit, Failure> + Send>;

/// An accepted update.
pub struct Submitted<V> {
    pub view: V,
    /// The request was already accepted under this id: nothing new runs.
    pub already_applied: bool,
}

pub(in crate::areas) struct Updates<V: UpdateView> {
    table: Mutex<Operations<V>>,
    tasks: AreaTasks,
    publish: Box<dyn Fn(V) + Send + Sync>,
    hook: Option<UpdateHook>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<V: UpdateView + Send + 'static> Updates<V> {
    pub(in crate::areas) fn new(
        tasks: AreaTasks,
        publish: impl Fn(V) + Send + Sync + 'static,
        hook: Option<UpdateHook>,
    ) -> Arc<Self> {
        Arc::new(Self {
            table: Mutex::new(Operations::new(MAX_UPDATES)),
            tasks,
            publish: Box::new(publish),
            hook,
        })
    }

    /// Accept an update: start it, or answer from the table.
    ///
    /// `pending` builds the pending view from the shared part.
    ///
    /// # Errors
    /// `Conflict` for an id used by a different request, `LimitExceeded` when
    /// every retained update is still running, `Draining` once the host stops.
    pub(in crate::areas) fn submit(
        self: &Arc<Self>,
        id: &OperationId,
        request: &[u8],
        pending: impl FnOnce(OperationView) -> V,
        prepare: Prepare,
        accepted: HostCommandPermit,
    ) -> Result<Submitted<V>, WorkbenchError> {
        let digest = digest_of(&[request]);
        let view = {
            let mut table = lock(&self.table);
            match table.lookup(id, &digest) {
                Lookup::Same(view) => {
                    return Ok(Submitted {
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
            let revision = table.next_revision();
            let view = pending(OperationView {
                operation_id: id.clone(),
                state: OperationState::Pending,
                revision,
                result: None,
            });
            table.insert(view.clone(), digest)?;
            view
        };
        let this = self.clone();
        let task_id = id.clone();
        let spawned =
            self.tasks
                .spawn_counted_tracked("management-update", Some(accepted), move |stop| {
                    this.run(task_id, prepare, stop)
                });
        if let Err(refused) = spawned {
            lock(&self.table).forget(id);
            return Err(WorkbenchError::new(
                ErrorCode::Draining,
                refused.to_string(),
                RetryAdvice::AfterReconnect,
            ));
        }
        Ok(Submitted {
            view,
            already_applied: false,
        })
    }

    /// One update, running or finished.
    pub(in crate::areas) fn get(&self, id: &OperationId) -> Option<V> {
        lock(&self.table).get(id).cloned()
    }

    async fn at(&self, stage: UpdateStage, id: &OperationId) {
        if let Some(hook) = &self.hook {
            hook(stage, id).await;
        }
    }

    async fn run(
        self: Arc<Self>,
        id: OperationId,
        prepare: Prepare,
        stop: StopSignal,
    ) -> Result<(), BootstrapError> {
        let mut guard = Settle {
            updates: self.clone(),
            id: id.clone(),
            settled: false,
        };
        let outcome = self.execute(&id, prepare, &stop).await;
        guard.finish(outcome);
        Ok(())
    }

    async fn execute(&self, id: &OperationId, prepare: Prepare, stop: &StopSignal) -> Settled {
        self.at(UpdateStage::Started, id).await;
        if stop.is_stopped() {
            return Settled::Cancelled(CancelReason::Shutdown);
        }
        let commit = match blocking(prepare).await {
            Ok(commit) => commit,
            Err(failure) => return Settled::Failed(failure),
        };
        self.at(UpdateStage::BeforeCommit, id).await;
        // The last look before the write: a stop that came meanwhile means the
        // result of this task belongs to a core that is going away.
        if stop.is_stopped() {
            return Settled::Cancelled(CancelReason::Shutdown);
        }
        match blocking(commit).await {
            Ok(done) => Settled::Done(done),
            Err(failure) => Settled::Failed(failure),
        }
    }

    fn settle(&self, id: &OperationId, outcome: &Settled) {
        let view = lock(&self.table).update(id, |view| view.settle(outcome));
        if let Some(view) = view {
            (self.publish)(view);
        }
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, Failure> + Send + 'static,
) -> Result<T, Failure> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| Failure::new(ErrorCode::Internal, "the update task did not finish"))?
}

/// Settles an update as cancelled when its task is dropped without an outcome.
struct Settle<V: UpdateView + Send + 'static> {
    updates: Arc<Updates<V>>,
    id: OperationId,
    settled: bool,
}

impl<V: UpdateView + Send + 'static> Settle<V> {
    fn finish(&mut self, outcome: Settled) {
        self.settled = true;
        self.updates.settle(&self.id, &outcome);
    }
}

impl<V: UpdateView + Send + 'static> Drop for Settle<V> {
    fn drop(&mut self) {
        if !self.settled {
            self.updates
                .settle(&self.id, &Settled::Cancelled(CancelReason::Shutdown));
        }
    }
}
