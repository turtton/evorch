//! CLI invocations the core serves (ADR 0045 §6, §10).
//!
//! An invocation owns what it started: one run incarnation and that run's
//! subtree. It ends when the client says so (Ctrl-C, the end of the command
//! line) or when the client's last connection ends. Ending stops exactly the
//! owned run with its subtree. Input the invocation sent to a run it does not
//! own is withdrawn while it is still queued and detached once the run took it;
//! that run is never stopped.
//!
//! * **Restart.** A planned restart cannot carry an invocation over yet (5b-R).
//!   Each running invocation holds a [`HostInvocationLease`] of the restart
//!   gate, which refuses the restart before it takes the gate or captures
//!   anything. Admission and the refusal share the gate's lock.
//! * **No resend.** Nothing here sends a command again. An input whose fate
//!   cannot be read is detached, and the end of an invocation is idempotent.
//! * **No resurrection.** The record lives in memory only: after a crash no
//!   invocation, and no run it owned, is brought back by this module.
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::Duration;

use event_bus::AgentRunPhase;
use runtime::{AgentRuntime, RunId, StopScope};
use workbench_protocol::ids::{ClientId, InvocationId};
use workbench_protocol::namespaces::execution::invocation::{
    InvocationEnded, InvocationInput, InvocationOwnership, InvocationState, InvocationView,
    MAX_INVOCATION_FINAL_BYTES,
};
use workbench_protocol::views::InputReceiptState;

use crate::bootstrap::restart::{HostInvocationLease, HostRestartGate, RestartStopHandler};
use crate::connections::{ConnectionEpoch, ConnectionHooks};

/// Ended invocations kept for a late read of their result.
const KEPT_ENDED: usize = 64;
/// Attempts to stop what a vanished client started, and the pause between them.
const DISCONNECT_ATTEMPTS: u32 = 5;
const DISCONNECT_RETRY: Duration = Duration::from_millis(200);

/// The lifecycle owner's stop path, installed after the host is built.
#[derive(Clone, Default)]
pub struct StopSlot(Arc<RwLock<Option<Arc<dyn RestartStopHandler>>>>);

impl StopSlot {
    pub fn get(&self) -> Option<Arc<dyn RestartStopHandler>> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn set(&self, handler: Arc<dyn RestartStopHandler>) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = Some(handler);
    }
}

/// Why a request about an invocation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvocationError {
    NotFound,
    /// The invocation belongs to another client.
    NotOwner,
    /// The core could not stop the run; the invocation is still running.
    StopFailed,
}

/// What is already known about an invocation id.
#[derive(Debug, Clone)]
pub struct Existing {
    pub fingerprint: String,
    pub client: ClientId,
    pub thread: String,
    pub run: RunId,
    pub ownership: InvocationOwnership,
}

/// An input the invocation sent to a run it does not own.
#[derive(Debug, Clone)]
pub struct SentInput {
    pub run: RunId,
    pub submission: String,
}

/// Everything the registry keeps about a new invocation.
pub struct NewInvocation {
    pub id: InvocationId,
    pub fingerprint: String,
    pub client: ClientId,
    pub epoch: ConnectionEpoch,
    pub thread: String,
    pub owned: Option<RunId>,
    pub input: Option<SentInput>,
    pub lease: HostInvocationLease,
}

struct Settled {
    state: InvocationState,
    final_text: Option<String>,
    detail: Option<String>,
}

struct Record {
    fingerprint: String,
    client: ClientId,
    epoch: ConnectionEpoch,
    thread: String,
    owned: Option<RunId>,
    /// The execution of `owned` this invocation started. A resume of the same
    /// run id is another incarnation and is not ours to stop.
    incarnation: Option<String>,
    input: Option<SentInput>,
    lease: Option<HostInvocationLease>,
    ended: Option<InvocationEnded>,
    settled: Option<Settled>,
}

#[derive(Default)]
struct State {
    records: BTreeMap<String, Record>,
    order: VecDeque<String>,
}

/// The invocations of one core.
pub struct InvocationRegistry {
    state: Mutex<State>,
    runtime: AgentRuntime,
    handle: tokio::runtime::Handle,
    gate: HostRestartGate,
    hook_installed: AtomicBool,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl InvocationRegistry {
    pub fn new(
        runtime: AgentRuntime,
        handle: tokio::runtime::Handle,
        gate: HostRestartGate,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            runtime,
            handle,
            gate,
            hook_installed: AtomicBool::new(false),
        })
    }

    /// Stop what a client started when its last connection ends. Installed once,
    /// before the first invocation is recorded.
    pub fn install_hook(self: &Arc<Self>, connections: &ConnectionHooks) {
        if self.hook_installed.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(self);
        connections.register(Arc::new(move |client, ended| {
            if let Some(registry) = weak.upgrade() {
                registry.client_left(client, ended);
            }
        }));
    }

    /// The restart gate invocations are admitted through.
    pub fn admit(&self) -> Result<HostInvocationLease, runtime::RuntimeError> {
        self.gate.enter_invocation()
    }

    /// Running invocations (a lease is held for each).
    pub fn running(&self) -> usize {
        self.gate.running_invocations()
    }

    pub fn find(&self, id: &InvocationId) -> Option<Existing> {
        let state = lock(&self.state);
        let record = state.records.get(id.as_str())?;
        let (run, ownership) = match (&record.owned, &record.input) {
            (Some(run), _) => (*run, InvocationOwnership::OwnsRun),
            (None, Some(input)) => (input.run, InvocationOwnership::InputOnly),
            (None, None) => return None,
        };
        Some(Existing {
            fingerprint: record.fingerprint.clone(),
            client: record.client.clone(),
            thread: record.thread.clone(),
            run,
            ownership,
        })
    }

    /// Record a started invocation. A connection that ended since `epoch` was
    /// taken leaves nothing running: the invocation is ended at once.
    pub fn record(self: &Arc<Self>, new: NewInvocation, connections: &ConnectionHooks) {
        let id = new.id.as_str().to_owned();
        let client = new.client.clone();
        let epoch = new.epoch;
        let owned = new.owned;
        let incarnation = owned.and_then(|run| self.runtime.run_incarnation(run).ok());
        {
            let mut state = lock(&self.state);
            state.records.insert(
                id.clone(),
                Record {
                    fingerprint: new.fingerprint,
                    client: new.client,
                    epoch: new.epoch,
                    thread: new.thread,
                    owned: new.owned,
                    incarnation,
                    input: new.input,
                    lease: Some(new.lease),
                    ended: None,
                    settled: None,
                },
            );
            state.order.push_back(id.clone());
            self.prune(&mut state);
        }
        if let Some(run) = owned {
            self.watch(id.clone(), run);
        }
        if connections.epoch_of(&client) != Some(epoch) {
            self.client_left(&client, epoch);
        }
    }

    /// Drop the oldest ended records beyond [`KEPT_ENDED`].
    fn prune(&self, state: &mut State) {
        let ended = state
            .records
            .values()
            .filter(|record| record.ended.is_some())
            .count();
        let mut excess = ended.saturating_sub(KEPT_ENDED);
        let mut index = 0;
        while excess > 0 && index < state.order.len() {
            let id = state.order[index].clone();
            if state
                .records
                .get(&id)
                .is_some_and(|record| record.ended.is_some())
            {
                state.records.remove(&id);
                state.order.remove(index);
                excess -= 1;
            } else {
                index += 1;
            }
        }
    }

    /// Learn how the owned run ended, for a client that reads late, and stop
    /// holding the restart gate once nothing runs.
    fn watch(self: &Arc<Self>, id: String, run: RunId) {
        let registry = self.clone();
        self.handle.spawn(async move {
            let phase = registry.runtime.wait(run).await;
            let text = registry.runtime.run_result(run).ok().flatten();
            registry.settle(&id, phase.ok(), text);
        });
    }

    fn settle(&self, id: &str, phase: Option<AgentRunPhase>, text: Option<String>) {
        let mut state = lock(&self.state);
        if let Some(record) = state.records.get_mut(id) {
            record.settled = Some(settled(phase, text));
            record.lease = None;
        }
    }

    /// Stop the run with its subtree, but only while it is still the execution
    /// this invocation started. The check and the stop are one step in the
    /// runtime, taken under the run's restore gate that a resume also takes, so
    /// a run resumed since (another incarnation) and its children are left alone.
    /// `Ok(false)` means nothing was stopped for that reason. The stop goes
    /// through the boundary the lifecycle owner installed in the runtime.
    fn stop_run(
        &self,
        run: RunId,
        incarnation: Option<&str>,
    ) -> Result<bool, runtime::RuntimeError> {
        let Some(incarnation) = incarnation else {
            return Ok(false);
        };
        match self
            .runtime
            .stop_incarnation(run, incarnation, StopScope::Subtree)
        {
            Ok(()) => Ok(true),
            Err(runtime::RuntimeError::RunTerminated { .. }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// End an invocation on behalf of its client.
    ///
    /// # Errors
    /// [`InvocationError`]; on `StopFailed` nothing ended and the call may be
    /// repeated (the end is idempotent).
    pub fn end(
        &self,
        id: &InvocationId,
        caller: &ClientId,
    ) -> Result<InvocationEnded, InvocationError> {
        let mut state = lock(&self.state);
        let record = state
            .records
            .get_mut(id.as_str())
            .ok_or(InvocationError::NotFound)?;
        if &record.client != caller {
            return Err(InvocationError::NotOwner);
        }
        self.end_record(id, record)
    }

    fn end_record(
        &self,
        id: &InvocationId,
        record: &mut Record,
    ) -> Result<InvocationEnded, InvocationError> {
        if let Some(done) = &record.ended {
            return Ok(InvocationEnded {
                already_ended: true,
                ..done.clone()
            });
        }
        let mut stopped = 0_u32;
        let mut withdrawn = 0_u32;
        let mut detached = 0_u32;
        if let Some(run) = record.owned {
            let live = self.runtime.live_descendants(run);
            // An admission-pending run has no inspection yet: it is stopped too.
            let own_live = !matches!(
                self.runtime.inspect_agent(run).map(|agent| agent.phase),
                Ok(AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped)
            );
            if own_live || !live.is_empty() {
                match self.stop_run(run, record.incarnation.as_deref()) {
                    Ok(false) => {}
                    Ok(true) => {
                        stopped =
                            u32::from(own_live) + u32::try_from(live.len()).unwrap_or(u32::MAX);
                    }
                    Err(runtime::RuntimeError::UnknownRun { .. }) => {}
                    Err(error) => {
                        tracing::warn!(%error, "could not stop the run of an invocation");
                        return Err(InvocationError::StopFailed);
                    }
                }
            }
        }
        if let Some(input) = &record.input {
            match self
                .runtime
                .withdraw_follow_up(input.run, &input.submission)
            {
                Ok(()) => withdrawn += 1,
                // Taken already, or not readable: the run is left alone and the
                // input is never sent again.
                Err(_) => detached += 1,
            }
        }
        record.lease = None;
        let done = InvocationEnded {
            invocation: id.clone(),
            stopped_runs: stopped,
            withdrawn_inputs: withdrawn,
            detached_inputs: detached,
            already_ended: false,
        };
        record.ended = Some(done.clone());
        Ok(done)
    }

    /// The last connection of `client` ended (epoch `ended`): end what it
    /// started in that epoch or before. Runs on the connection's teardown, so it
    /// only hands the work to a task.
    fn client_left(self: &Arc<Self>, client: &ClientId, ended: ConnectionEpoch) {
        let ids: Vec<String> = {
            let state = lock(&self.state);
            state
                .records
                .iter()
                .filter(|(_, record)| {
                    &record.client == client && record.epoch <= ended && record.ended.is_none()
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        if ids.is_empty() {
            return;
        }
        let registry = self.clone();
        self.handle.spawn(async move {
            for id in ids {
                let Ok(invocation) = InvocationId::new(&id) else {
                    continue;
                };
                for attempt in 0..DISCONNECT_ATTEMPTS {
                    let this = registry.clone();
                    let target = invocation.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        let mut state = lock(&this.state);
                        match state.records.get_mut(target.as_str()) {
                            Some(record) => this.end_record(&target, record),
                            None => Err(InvocationError::NotFound),
                        }
                    })
                    .await;
                    match result {
                        Ok(Err(InvocationError::StopFailed))
                            if attempt + 1 < DISCONNECT_ATTEMPTS =>
                        {
                            tokio::time::sleep(DISCONNECT_RETRY).await;
                        }
                        _ => break,
                    }
                }
            }
        });
    }

    /// Where an invocation stands.
    ///
    /// # Errors
    /// [`InvocationError::NotFound`] and [`InvocationError::NotOwner`].
    pub fn view(
        &self,
        id: &InvocationId,
        caller: &ClientId,
    ) -> Result<InvocationView, InvocationError> {
        let (record_client, thread, owned, input, ended, settled) = {
            let state = lock(&self.state);
            let record = state
                .records
                .get(id.as_str())
                .ok_or(InvocationError::NotFound)?;
            (
                record.client.clone(),
                record.thread.clone(),
                record.owned,
                record.input.clone(),
                record.ended.is_some(),
                record.settled.as_ref().map(|s| Settled {
                    state: s.state,
                    final_text: s.final_text.clone(),
                    detail: s.detail.clone(),
                }),
            )
        };
        if &record_client != caller {
            return Err(InvocationError::NotOwner);
        }
        let thread = workbench_protocol::ids::ThreadId::new(thread)
            .map_err(|_| InvocationError::NotFound)?;
        let mut view = InvocationView {
            invocation: id.clone(),
            thread,
            state: InvocationState::Running,
            owned_run: owned
                .and_then(|run| workbench_protocol::ids::RunId::new(run.to_string()).ok()),
            input: None,
            final_text: None,
            detail: None,
        };
        if let Some(run) = owned {
            let (state, text, detail) = self.owned_state(run, settled, ended);
            view.state = state;
            view.final_text = text;
            view.detail = detail;
        } else if let Some(input) = input {
            let (state, receipt, detail) = self.input_state(&input);
            view.state = if ended && !state.is_terminal() {
                InvocationState::Ended
            } else {
                state
            };
            view.detail = detail;
            if let (Ok(run), Ok(submission)) = (
                workbench_protocol::ids::RunId::new(input.run.to_string()),
                workbench_protocol::ids::SubmissionId::new(&input.submission),
            ) {
                view.input = Some(InvocationInput {
                    run,
                    submission,
                    receipt,
                });
            }
        }
        Ok(view)
    }

    fn owned_state(
        &self,
        run: RunId,
        settled: Option<Settled>,
        ended: bool,
    ) -> (InvocationState, Option<String>, Option<String>) {
        let live = match self.runtime.inspect_agent(run) {
            Ok(agent) => Some(agent.phase),
            Err(_) => None,
        };
        match live {
            Some(AgentRunPhase::Pending | AgentRunPhase::Running) => {
                (InvocationState::Running, None, None)
            }
            Some(AgentRunPhase::Waiting) => {
                if self.blocked_on_user(run) {
                    (InvocationState::WaitingForInput, None, None)
                } else {
                    (InvocationState::Running, None, None)
                }
            }
            Some(phase @ (AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped)) => {
                let text = self.runtime.run_result(run).ok().flatten();
                let s = settled_from(phase, text);
                (s.state, s.final_text, s.detail)
            }
            None => match settled {
                Some(s) => (s.state, s.final_text, s.detail),
                None if ended => (InvocationState::Ended, None, None),
                None => (
                    InvocationState::Failed,
                    None,
                    Some("the core no longer knows the run".into()),
                ),
            },
        }
    }

    fn blocked_on_user(&self, run: RunId) -> bool {
        self.runtime.user_answers(run).is_ok_and(|questions| {
            questions
                .iter()
                .any(|question| question.blocking && question.is_pending())
        })
    }

    fn input_state(
        &self,
        input: &SentInput,
    ) -> (InvocationState, InputReceiptState, Option<String>) {
        let status = self
            .runtime
            .with_storage_reader(|db| {
                Ok(db
                    .host_input_receipt_headers(&input.run.to_string())?
                    .into_iter()
                    .find(|header| header.prompt_id == input.submission)
                    .map(|header| header.status))
            })
            .ok()
            .flatten()
            .flatten();
        match status {
            Some(storage::HostInputStatus::Accepted) => {
                (InvocationState::Running, InputReceiptState::Accepted, None)
            }
            Some(storage::HostInputStatus::Delivered) => (
                InvocationState::Succeeded,
                InputReceiptState::Delivered,
                None,
            ),
            Some(storage::HostInputStatus::Withdrawn) => {
                (InvocationState::Ended, InputReceiptState::Withdrawn, None)
            }
            Some(storage::HostInputStatus::Dropped) => (
                InvocationState::Failed,
                InputReceiptState::Dropped,
                Some("the run ended before it read the input".into()),
            ),
            Some(storage::HostInputStatus::Unknown) | None => (
                InvocationState::Failed,
                InputReceiptState::Unknown,
                Some("the core cannot tell whether the run received the input".into()),
            ),
        }
    }
}

fn settled(phase: Option<AgentRunPhase>, text: Option<String>) -> Settled {
    match phase {
        Some(phase) => settled_from(phase, text),
        None => Settled {
            state: InvocationState::Failed,
            final_text: None,
            detail: Some("the core lost track of the run".into()),
        },
    }
}

fn settled_from(phase: AgentRunPhase, text: Option<String>) -> Settled {
    match phase {
        AgentRunPhase::Done => Settled {
            state: InvocationState::Succeeded,
            final_text: text.map(bound),
            detail: None,
        },
        AgentRunPhase::Stopped => Settled {
            state: InvocationState::Stopped,
            final_text: None,
            detail: Some("the run was stopped".into()),
        },
        _ => Settled {
            state: InvocationState::Failed,
            final_text: text.map(bound),
            detail: Some("the run failed".into()),
        },
    }
}

/// The final text, cut at a character boundary below the wire bound.
fn bound(mut text: String) -> String {
    if text.len() > MAX_INVOCATION_FINAL_BYTES {
        let mut end = MAX_INVOCATION_FINAL_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}
