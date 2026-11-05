//! Core-local approval authority. Events are projections, never authorization.
//!
//! Subscribe before listing. On lag, resnapshot pending rows. Terminal tombstones
//! are bounded; after eviction old decisions return Unknown, never become Pending.
use event_bus::{ApprovalCorrelation, Event, EventBus, ToolEvent};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{broadcast, oneshot},
    time::Instant,
};
use uuid::Uuid;

pub const SUMMARY_MAX_BYTES: usize = 8192;
/// Full confirmation payloads are capped separately from list summaries. Larger
/// requests are cancelled before publication, never approved from a projection.
pub const FULL_INPUT_MAX_BYTES: usize = 1024 * 1024;
pub const TOOL_NAME_MAX_BYTES: usize = 256;
pub const PAGE_MAX: usize = 100;
const TOMBSTONES_MAX: usize = 1024;
type OwnedApprovals = Arc<Mutex<BTreeMap<String, (Weak<ApprovalRegistry>, ApprovalId)>>>;

/// Opaque request token plus its issuing core epoch; not a provider call ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalId {
    pub core_epoch: u64,
    pub value: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalState {
    Pending,
    Approved,
    Denied,
    Expired,
    Cancelled,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    Deny,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalActor {
    Human,
    Agent,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalSurface {
    Desktop,
    Web,
    Cli,
    None,
}
/// Attached by trusted ingress, not by tool arguments or the client payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApprovalProvenance {
    pub actor: ApprovalActor,
    pub surface: ApprovalSurface,
}
impl ApprovalProvenance {
    pub const DESKTOP: Self = Self {
        actor: ApprovalActor::Human,
        surface: ApprovalSurface::Desktop,
    };
    pub const CLI: Self = Self {
        actor: ApprovalActor::Human,
        surface: ApprovalSurface::Cli,
    };
}
#[derive(Debug, Clone)]
pub struct ApprovalSnapshot {
    pub id: ApprovalId,
    pub revision: u64,
    pub run_id: String,
    pub thread_id: Option<String>,
    pub call_id: String,
    /// Core-monotonic registration attempt, independent of the provider ID.
    pub attempt: u64,
    /// The runtime execution generation; changes when a durable run is resumed.
    pub execution_attempt: Option<Uuid>,
    pub tool_name: String,
    pub summary: String,
    /// Serialized complete argument size (zero for no arguments).
    pub input_bytes: usize,
    pub deadline: Instant,
    pub state: ApprovalState,
    pub decided_by: Option<ApprovalProvenance>,
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApprovalError {
    #[error("approval belongs to another core epoch")]
    WrongEpoch,
    #[error("unknown approval")]
    Unknown,
    #[error("approval already decided: {0:?}")]
    AlreadyDecided(ApprovalState),
    #[error("approval expired")]
    Expired,
    #[error("approval cancelled")]
    Cancelled,
    #[error("complete approval input exceeds {FULL_INPUT_MAX_BYTES} bytes ({bytes} bytes)")]
    InputTooLarge { bytes: usize },
    #[error("approval revision mismatch (current: {current})")]
    RevisionMismatch { current: u64 },
}
/// Arguments are retained only while pending, for complete confirmation. List
/// snapshots contain a bounded summary; use `full_input` for the complete value.
pub struct ApprovalRequest<'a> {
    pub run_id: &'a str,
    pub thread_id: Option<&'a str>,
    pub tool_name: &'a str,
    pub call_id: &'a str,
    pub input: Option<serde_json::Value>,
}

/// One execution lifetime. Stop, Cancel, owner fencing and last-owner drop close
/// pending requests synchronously, without requiring an executor poll.
pub struct ApprovalOwner {
    attempt: Uuid,
    closed: Arc<AtomicBool>,
    pending: OwnedApprovals,
}
impl Default for ApprovalOwner {
    fn default() -> Self {
        Self {
            attempt: Uuid::new_v4(),
            closed: Arc::default(),
            pending: Arc::default(),
        }
    }
}
impl ApprovalOwner {
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let pending = std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for (_, (registry, id)) in pending {
            if let Some(registry) = registry.upgrade() {
                registry.cancel(&id);
            }
        }
    }
}
impl Drop for ApprovalOwner {
    fn drop(&mut self) {
        self.close();
    }
}
struct Entry {
    snapshot: ApprovalSnapshot,
    input: Option<Arc<serde_json::Value>>,
    owner: Option<Binding>,
    waiter: Option<oneshot::Sender<ApprovalState>>,
}
#[derive(Clone)]
struct Binding {
    owner: Weak<ApprovalOwner>,
    attempt: Uuid,
    closed: Arc<AtomicBool>,
    pending: OwnedApprovals,
    thread: Option<String>,
}
#[derive(Default)]
struct State {
    next: u64,
    entries: BTreeMap<String, Entry>,
    terminal: VecDeque<String>,
    bindings: BTreeMap<String, Binding>,
}
pub struct ApprovalRegistry {
    core_epoch: u64,
    bus: Weak<EventBus>,
    state: Mutex<State>,
    changes: broadcast::Sender<ApprovalSnapshot>,
}
impl ApprovalRegistry {
    /// One registry per core bus. Never reconstruct authority from replayed events.
    pub fn for_bus(bus: &Arc<EventBus>) -> Arc<Self> {
        bus.shared_state(|| Self {
            core_epoch: u64::from_be_bytes(
                Uuid::new_v4().as_bytes()[..8]
                    .try_into()
                    .expect("eight bytes"),
            ),
            bus: Arc::downgrade(bus),
            state: Mutex::default(),
            changes: broadcast::channel(256).0,
        })
    }
    pub fn existing(bus: &EventBus) -> Option<Arc<Self>> {
        bus.get_shared_state()
    }
    pub const fn core_epoch(&self) -> u64 {
        self.core_epoch
    }
    /// Subscribe first, then list. Hosts may discard duplicate revisions.
    pub fn subscribe(&self) -> broadcast::Receiver<ApprovalSnapshot> {
        self.changes.subscribe()
    }
    /// Rebinding a durable run to a new execution invalidates its old pending rows.
    pub fn bind_run(&self, run: &str, thread: Option<String>, owner: &Arc<ApprovalOwner>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ids: Vec<_> = state
            .entries
            .values()
            .filter(|e| e.snapshot.run_id == run && e.snapshot.state == ApprovalState::Pending)
            .map(|e| e.snapshot.id.value.clone())
            .collect();
        for id in ids {
            self.finish(&mut state, &id, ApprovalState::Cancelled, None);
        }
        state.bindings.insert(
            run.into(),
            Binding {
                owner: Arc::downgrade(owner),
                attempt: owner.attempt,
                closed: owner.closed.clone(),
                pending: owner.pending.clone(),
                thread,
            },
        );
    }
    pub fn register(
        self: &Arc<Self>,
        request: ApprovalRequest<'_>,
        timeout: Duration,
    ) -> PendingApproval {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let binding = state.bindings.get(request.run_id).cloned();
        state.next = state
            .next
            .checked_add(1)
            .expect("approval attempt exhausted");
        let id = ApprovalId {
            core_epoch: self.core_epoch,
            value: format!(
                "{}:{}:{}:{}",
                scoped_label(request.run_id, 48),
                scoped_label(request.call_id, 16),
                state.next,
                Uuid::new_v4()
            ),
        };
        // close() releases the owner lock before entering a registry, so there
        // is no owner -> registry lock inversion. Recheck closed before publish.
        if let Some(owner) = &binding {
            owner
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(id.value.clone(), (Arc::downgrade(self), id.clone()));
        }
        let (waiter, receiver) = oneshot::channel();
        let serialized = request
            .input
            .as_ref()
            .map_or_else(String::new, serde_json::Value::to_string);
        let input_bytes = serialized.len();
        let summary = bounded(&serialized, SUMMARY_MAX_BYTES);
        let tool_name = bounded(request.tool_name, TOOL_NAME_MAX_BYTES);
        let input = if input_bytes <= FULL_INPUT_MAX_BYTES {
            request.input.map(Arc::new)
        } else {
            None
        };
        let snapshot = ApprovalSnapshot {
            id: id.clone(),
            revision: 1,
            run_id: request.run_id.into(),
            thread_id: binding
                .as_ref()
                .and_then(|b| b.thread.clone())
                .or_else(|| request.thread_id.map(str::to_owned)),
            call_id: request.call_id.into(),
            attempt: state.next,
            execution_attempt: binding.as_ref().map(|b| b.attempt),
            tool_name: tool_name.clone(),
            summary,
            input_bytes,
            deadline: Instant::now() + timeout,
            state: ApprovalState::Pending,
            decided_by: None,
        };
        state.entries.insert(
            id.value.clone(),
            Entry {
                snapshot: snapshot.clone(),
                input: input.clone(),
                owner: binding,
                waiter: Some(waiter),
            },
        );
        let pending = PendingApproval {
            registry: Arc::downgrade(self),
            id: id.clone(),
            deadline: snapshot.deadline,
            receiver: Some(receiver),
        };
        // The receiver and drop guard already exist before Pending is published.
        if input_bytes > FULL_INPUT_MAX_BYTES || Self::owner_gone(&state.entries[&id.value]) {
            self.finish(&mut state, &id.value, ApprovalState::Cancelled, None);
        } else {
            let correlation = Self::correlation(&snapshot);
            let _ = self.changes.send(snapshot);
            if let Some(bus) = self.bus.upgrade() {
                bus.emit(Event::new(ToolEvent::ApprovalRequested {
                    tool_name,
                    call_id: id.value.clone(),
                    correlation: Some(correlation),
                    // The existing desktop renderer receives the COMPLETE input.
                    input: input.as_deref().cloned(),
                }));
            }
        }
        pending
    }
    /// The sole decision ingress. Terminal status takes precedence over a stale
    /// revision. Deadline/owner checks and Pending -> terminal are one CAS lock.
    pub fn resolve(
        &self,
        id: &ApprovalId,
        expected_revision: u64,
        decision: ApprovalDecision,
        provenance: ApprovalProvenance,
    ) -> Result<ApprovalSnapshot, ApprovalError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.pending_entry(&mut state, id, expected_revision)?;
        let next = match decision {
            ApprovalDecision::Approve => ApprovalState::Approved,
            ApprovalDecision::Deny => ApprovalState::Denied,
        };
        self.finish(&mut state, &id.value, next, Some(provenance));
        Ok(state.entries[&id.value].snapshot.clone())
    }
    /// Fetch the immutable COMPLETE arguments for confirmation, never the list
    /// summary. Hosts must authorize the caller before fetching, and show this
    /// value before offering Approve. None means the request has no arguments.
    /// Epoch/revision, deadline and owner are checked like the decision ingress.
    pub fn full_input(
        &self,
        id: &ApprovalId,
        expected_revision: u64,
    ) -> Result<Option<Arc<serde_json::Value>>, ApprovalError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(self
            .pending_entry(&mut state, id, expected_revision)?
            .input
            .clone())
    }
    fn pending_entry<'s>(
        &self,
        state: &'s mut State,
        id: &ApprovalId,
        expected_revision: u64,
    ) -> Result<&'s Entry, ApprovalError> {
        if id.core_epoch != self.core_epoch {
            return Err(ApprovalError::WrongEpoch);
        }
        self.refresh(state, &id.value);
        let snapshot = &state
            .entries
            .get(&id.value)
            .ok_or(ApprovalError::Unknown)?
            .snapshot;
        if snapshot.input_bytes > FULL_INPUT_MAX_BYTES {
            return Err(ApprovalError::InputTooLarge {
                bytes: snapshot.input_bytes,
            });
        }
        match snapshot.state {
            ApprovalState::Pending => {}
            ApprovalState::Expired => return Err(ApprovalError::Expired),
            ApprovalState::Cancelled => return Err(ApprovalError::Cancelled),
            decided => return Err(ApprovalError::AlreadyDecided(decided)),
        }
        if snapshot.revision != expected_revision {
            return Err(ApprovalError::RevisionMismatch {
                current: snapshot.revision,
            });
        }
        Ok(&state.entries[&id.value])
    }
    fn correlation(snapshot: &ApprovalSnapshot) -> ApprovalCorrelation {
        ApprovalCorrelation {
            run_id: snapshot.run_id.clone(),
            call_id: snapshot.call_id.clone(),
            attempt: snapshot.attempt,
        }
    }
    /// Legacy desktop/CLI bridge: call_id now carries the opaque registry token.
    /// It never looks up a provider call ID. New hosts must quote epoch/revision.
    pub fn resolve_presented(
        &self,
        token: &str,
        approved: bool,
        provenance: ApprovalProvenance,
    ) -> Result<ApprovalSnapshot, ApprovalError> {
        self.resolve(
            &ApprovalId {
                core_epoch: self.core_epoch,
                value: token.into(),
            },
            1,
            if approved {
                ApprovalDecision::Approve
            } else {
                ApprovalDecision::Deny
            },
            provenance,
        )
    }
    pub fn get(&self, id: &ApprovalId) -> Option<ApprovalSnapshot> {
        if id.core_epoch != self.core_epoch {
            return None;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.refresh(&mut state, &id.value);
        state.entries.get(&id.value).map(|e| e.snapshot.clone())
    }
    /// Keyset pages: use the last attempt as after. Concurrent removals cannot
    /// resurrect rows. Host cursors must also bind the registry's core epoch.
    pub fn list_pending(&self, after: Option<u64>, limit: usize) -> Vec<ApprovalSnapshot> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ids: Vec<_> = state.entries.keys().cloned().collect();
        for id in ids {
            self.refresh(&mut state, &id);
        }
        let mut rows: Vec<_> = state
            .entries
            .values()
            .filter(|e| {
                e.snapshot.state == ApprovalState::Pending
                    && after.is_none_or(|a| e.snapshot.attempt > a)
            })
            .map(|e| e.snapshot.clone())
            .collect();
        rows.sort_by_key(|s| s.attempt);
        rows.truncate(limit.min(PAGE_MAX));
        rows
    }
    fn owner_gone(entry: &Entry) -> bool {
        entry
            .owner
            .as_ref()
            .is_some_and(|b| b.owner.strong_count() == 0 || b.closed.load(Ordering::Acquire))
    }
    fn refresh(&self, state: &mut State, id: &str) {
        let Some(entry) = state.entries.get(id) else {
            return;
        };
        if entry.snapshot.state != ApprovalState::Pending {
            return;
        }
        let next = if Instant::now() >= entry.snapshot.deadline {
            Some(ApprovalState::Expired)
        } else if Self::owner_gone(entry) {
            Some(ApprovalState::Cancelled)
        } else {
            None
        };
        if let Some(next) = next {
            self.finish(state, id, next, None);
        }
    }
    fn cancel(&self, id: &ApprovalId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.refresh(&mut state, &id.value);
        self.finish(&mut state, &id.value, ApprovalState::Cancelled, None);
    }
    fn finish(
        &self,
        state: &mut State,
        id: &str,
        next: ApprovalState,
        provenance: Option<ApprovalProvenance>,
    ) {
        let Some(entry) = state.entries.get_mut(id) else {
            return;
        };
        if entry.snapshot.state != ApprovalState::Pending {
            return;
        }
        // Terminal tombstones retain metadata, not potentially large arguments.
        entry.input = None;
        entry.snapshot.state = next;
        entry.snapshot.revision += 1;
        entry.snapshot.decided_by = provenance;
        if let Some(owner) = &entry.owner {
            owner
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(id);
        }
        let _ = self.changes.send(entry.snapshot.clone());
        if let Some(waiter) = entry.waiter.take() {
            let _ = waiter.send(next);
        }
        if let Some(bus) = self.bus.upgrade() {
            bus.emit(Event::new(ToolEvent::ApprovalResolved {
                call_id: id.into(),
                correlation: Some(Self::correlation(&entry.snapshot)),
                approved: next == ApprovalState::Approved,
            }));
        }
        state.terminal.push_back(id.into());
        while state.terminal.len() > TOMBSTONES_MAX {
            if let Some(old) = state.terminal.pop_front() {
                state.entries.remove(&old);
            }
        }
    }
}
/// Cancel-on-drop wait handle. No detached task outlives the caller.
pub struct PendingApproval {
    registry: Weak<ApprovalRegistry>,
    pub(super) id: ApprovalId,
    deadline: Instant,
    receiver: Option<oneshot::Receiver<ApprovalState>>,
}
impl PendingApproval {
    pub const fn id(&self) -> &ApprovalId {
        &self.id
    }
    pub async fn wait(mut self) -> ApprovalState {
        let receiver = self.receiver.take().expect("wait once");
        tokio::pin!(receiver);
        tokio::select! {
            result = &mut receiver => result.unwrap_or(ApprovalState::Cancelled),
            _ = tokio::time::sleep_until(self.deadline) => {
                let Some(registry) = self.registry.upgrade() else { return ApprovalState::Cancelled; };
                // If resolve already won the CAS, return its result, not a timeout.
                let _ = registry.get(&self.id);
                receiver.await.unwrap_or(ApprovalState::Cancelled)
            }
        }
    }
}
impl Drop for PendingApproval {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            registry.cancel(&self.id);
        }
    }
}
// Labels only aid legacy GUI run routing. Full identities stay in the entry.
// 48+16+20+36+3 = 123 bytes, below protocol MAX_ID_BYTES (128).
fn scoped_label(value: &str, max: usize) -> String {
    bounded(value, max)
        .chars()
        .map(|c| if c.is_control() || c == ':' { '_' } else { c })
        .collect()
}
fn bounded(value: &str, max: usize) -> String {
    let mut end = value.len().min(max);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].into()
}
