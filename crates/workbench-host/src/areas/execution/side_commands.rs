//! Side questions (`/btw`) of the `execution` area (ADR 0045 E10).
//!
//! * **Handler and task.** `ask` takes the thread's frozen context from the
//!   runtime, records the pending operation and hands the provider call to a
//!   counted task that keeps the command's restart permit until the operation
//!   settles. The handler never waits for the provider and never opens storage.
//! * **One writer of the final state.** Only the task settles an operation: its
//!   answer, a requested cancel (a watch the cancel command raises) or the stop
//!   of the host. A task dropped without an outcome settles as
//!   `cancelled(shutdown)`.
//! * **Memory only.** The question and the answer stay in the operation table
//!   (bounded, shared with the other areas' operations). Nothing reaches the
//!   conversation, the input receipts or the events: `BtwChanged` only names
//!   the operation, and the text is read with `get`.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use runtime::btw::{BtwAnswer, BtwError, BtwSnapshot};
use runtime::{AgentRuntime, RunId as RuntimeRunId};
use tokio::sync::watch;
use workbench_protocol::errors::{ErrorCode, RetryAdvice, WorkbenchError};
use workbench_protocol::events::UiEvent;
use workbench_protocol::ids::{OperationId, RunId, ThreadId};
use workbench_protocol::namespaces::ExecutionEvent;
use workbench_protocol::namespaces::execution::side_commands::{
    AskBtw, BtwSource, BtwView, MAX_BTW_ANSWER_BYTES, MAX_BTW_FAILURE_BYTES,
};
use workbench_protocol::operation::{CancelReason, OperationState, OperationView};

use crate::areas::settings::operations::{Lookup, Operations, Tracked, digest_of};
use crate::areas::{AreaTasks, ProjectionSource, StopSignal};
use crate::bootstrap::BootstrapError;
use crate::bootstrap::restart::HostCommandPermit;
use crate::queries::{error, masked, text};

/// Side questions the core remembers (running ones are never dropped).
const MAX_BTW_OPERATIONS: usize = 32;

impl Tracked for BtwView {
    fn operation(&self) -> &OperationView {
        &self.operation
    }

    fn operation_mut(&mut self) -> &mut OperationView {
        &mut self.operation
    }
}

/// An accepted request.
pub struct AcceptedBtw {
    pub view: BtwView,
    /// The request was already accepted under this id: nothing new runs.
    pub already_applied: bool,
}

struct State {
    table: Operations<BtwView>,
    cancels: HashMap<OperationId, watch::Sender<bool>>,
}

struct Inner {
    state: Mutex<State>,
    tasks: AreaTasks,
    source: ProjectionSource,
}

/// The side questions of the `execution` area.
#[derive(Clone)]
pub struct SideQuestions {
    inner: Arc<Inner>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn unknown() -> WorkbenchError {
    error(
        ErrorCode::NotFound,
        "unknown side question; ask it again if the core restarted",
    )
}

/// The refusal of a question that cannot take its context.
fn context_error(failure: &BtwError) -> WorkbenchError {
    let code = match failure {
        BtwError::NoLocalContext | BtwError::OwnedElsewhere => ErrorCode::NotFound,
        BtwError::ContextNotReady => ErrorCode::Busy,
        BtwError::UnsupportedProvider => ErrorCode::UnsupportedCapability,
        BtwError::ContextTooLarge => ErrorCode::LimitExceeded,
        BtwError::ProviderFailed(_) | BtwError::UnexpectedToolCall | BtwError::EmptyAnswer => {
            ErrorCode::Internal
        }
    };
    error(code, &failure.to_string())
}

enum Ended {
    Answered(BtwAnswer),
    Failed(BtwError),
    Cancelled(CancelReason),
}

impl SideQuestions {
    pub(super) fn new(tasks: AreaTasks, source: ProjectionSource) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    table: Operations::new(MAX_BTW_OPERATIONS),
                    cancels: HashMap::new(),
                }),
                tasks,
                source,
            }),
        }
    }

    /// Accept a question: start it, or answer from the table. `root` is the run
    /// the thread's conversation currently has; it is only looked at for a new
    /// operation, so a repeat of an accepted request never takes a new context.
    ///
    /// # Errors
    /// `Conflict` for an id used by a different request, `NotFound` or `Busy`
    /// when the thread has no context to ask, `LimitExceeded` when every
    /// operation the table holds is still running, `Draining` once the host
    /// stops.
    pub fn ask(
        &self,
        runtime: &AgentRuntime,
        root: Option<RuntimeRunId>,
        ask: &AskBtw,
        accepted: HostCommandPermit,
    ) -> Result<AcceptedBtw, WorkbenchError> {
        let digest = digest_of(&[ask.thread.as_str().as_bytes(), ask.question.as_bytes()]);
        let (view, snapshot, cancel) = {
            let mut state = lock(&self.inner.state);
            match state.table.lookup(&ask.operation_id, &digest) {
                Lookup::Same(view) => {
                    return Ok(AcceptedBtw {
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
            let root = root.ok_or_else(|| {
                error(ErrorCode::NotFound, "the thread has no conversation to ask")
            })?;
            let snapshot = runtime
                .btw_snapshot(root)
                .map_err(|failure| context_error(&failure))?;
            let source = snapshot.source();
            let revision = state.table.next_revision();
            let view = BtwView {
                operation: OperationView {
                    operation_id: ask.operation_id.clone(),
                    state: OperationState::Pending,
                    revision,
                    result: None,
                },
                thread: ask.thread.clone(),
                source: RunId::new(source.run_id).ok().map(|run| BtwSource {
                    run,
                    settled: source.settled,
                }),
                answer: None,
                truncated: false,
                failure: None,
            };
            state.table.insert(view.clone(), digest)?;
            let (cancel, observed) = watch::channel(false);
            state.cancels.insert(ask.operation_id.clone(), cancel);
            (view, snapshot, observed)
        };
        let inner = self.inner.clone();
        let id = ask.operation_id.clone();
        let question = ask.question.clone();
        let spawned = self
            .inner
            .tasks
            .spawn_counted_tracked("btw", Some(accepted), move |stop| {
                inner.run(id, snapshot, question, stop, cancel)
            });
        if let Err(refused) = spawned {
            let mut state = lock(&self.inner.state);
            state.table.forget(&ask.operation_id);
            state.cancels.remove(&ask.operation_id);
            return Err(WorkbenchError::new(
                ErrorCode::Draining,
                refused.to_string(),
                RetryAdvice::AfterReconnect,
            ));
        }
        Ok(AcceptedBtw {
            view,
            already_applied: false,
        })
    }

    /// Ask a running question to stop; the view of one that already settled.
    /// The task settles the operation.
    ///
    /// # Errors
    /// `NotFound` for an operation the core does not hold.
    pub fn cancel(&self, id: &OperationId) -> Result<AcceptedBtw, WorkbenchError> {
        let state = lock(&self.inner.state);
        let view = state.table.get(id).cloned().ok_or_else(unknown)?;
        let already_applied = state
            .cancels
            .get(id)
            .is_none_or(|cancel| cancel.send_replace(true));
        Ok(AcceptedBtw {
            view,
            already_applied,
        })
    }

    /// One question, running or settled.
    ///
    /// # Errors
    /// `NotFound` for an operation the core does not hold.
    pub fn get(&self, id: &OperationId) -> Result<BtwView, WorkbenchError> {
        lock(&self.inner.state)
            .table
            .get(id)
            .cloned()
            .ok_or_else(unknown)
    }
}

impl Inner {
    async fn run(
        self: Arc<Self>,
        id: OperationId,
        snapshot: Arc<BtwSnapshot>,
        question: String,
        mut stop: StopSignal,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<(), BootstrapError> {
        let mut guard = Settle {
            inner: self.clone(),
            id,
            settled: false,
        };
        let ended = tokio::select! {
            biased;
            () = stop.stopped() => Ended::Cancelled(CancelReason::Shutdown),
            _ = cancel.wait_for(|requested| *requested) => Ended::Cancelled(CancelReason::Requested),
            answer = snapshot.ask(&question) => match answer {
                Ok(answer) => Ended::Answered(answer),
                Err(failure) => Ended::Failed(failure),
            },
        };
        guard.finish(ended);
        Ok(())
    }

    fn settle(&self, id: &OperationId, ended: Ended) {
        let changed = {
            let mut state = lock(&self.state);
            state.cancels.remove(id);
            state.table.update(id, |view| match ended {
                Ended::Answered(answer) => {
                    let full = masked(&answer.text);
                    let cut = text(&full, MAX_BTW_ANSWER_BYTES);
                    view.truncated = cut.len() < full.len();
                    view.answer = Some(cut);
                    view.operation.state = OperationState::Ready;
                }
                Ended::Failed(failure) => {
                    view.failure = Some(text(&failure.to_string(), MAX_BTW_FAILURE_BYTES));
                    view.operation.state = OperationState::Failed {
                        code: context_error(&failure).code,
                    };
                }
                Ended::Cancelled(reason) => {
                    view.operation.state = OperationState::Cancelled { reason };
                }
            })
        };
        if let Some(view) = changed {
            publish(&self.source, view.thread, view.operation);
        }
    }
}

fn publish(source: &ProjectionSource, thread: ThreadId, operation: OperationView) {
    let event = ExecutionEvent::BtwChanged { thread, operation };
    if let Err(failure) = source.publish(UiEvent::Execution(event)) {
        tracing::warn!(?failure.code, "side question change was not published");
    }
}

/// Settles an operation as cancelled when its task is dropped without an outcome.
struct Settle {
    inner: Arc<Inner>,
    id: OperationId,
    settled: bool,
}

impl Settle {
    fn finish(&mut self, ended: Ended) {
        self.settled = true;
        self.inner.settle(&self.id, ended);
    }
}

impl Drop for Settle {
    fn drop(&mut self) {
        if !self.settled {
            self.inner
                .settle(&self.id, Ended::Cancelled(CancelReason::Shutdown));
        }
    }
}
