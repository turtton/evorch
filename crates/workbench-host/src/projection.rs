//! Core-local read projection and its single publication boundary.
use crate::{
    queries::{self, Bodies, Models, Pager},
    subscriptions::{self, Subscriber, Subscription, SubscriptionLimits},
};
use event_bus::{EventKind, RecvError};
use runtime::AgentRuntime;
use sandbox::approval::{ApprovalRegistry, ApprovalSnapshot};
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use workbench_protocol::{
    envelope::Provenance,
    errors::{ErrorCode, WorkbenchError},
    events::*,
    ids::*,
    queries::{PageRequest, Query, QueryReply},
    views::*,
    wire::*,
};

struct Replay {
    cursor: EventCursor,
    event: UiEvent,
    bytes: usize,
}
struct Frozen {
    id: SnapshotId,
    items: Vec<SnapshotItem>,
    revision: Revision,
}
struct State {
    #[cfg(test)]
    snapshot_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    epoch: u64,
    sequence: u64,
    next_subscription: u64,
    scanner: queries::CommitScanner,
    initialized: bool,
    projects: Vec<(ProjectId, PathBuf)>,
    models: Models,
    replay: VecDeque<Replay>,
    replay_bytes: usize,
    subscribers: Vec<Subscriber>,
    snapshots: VecDeque<Frozen>,
    answer_provenance: BTreeMap<String, Provenance>,
}
impl State {
    fn next_cursor(&mut self) -> EventCursor {
        self.sequence = self
            .sequence
            .checked_add(1)
            .expect("event sequence exhausted");
        self.cursor()
    }
    fn cursor(&self) -> EventCursor {
        EventCursor {
            core_epoch: InstanceEpoch(self.epoch),
            sequence: Sequence(self.sequence),
        }
    }
    fn publish(&mut self, event: UiEvent, limits: SubscriptionLimits) {
        let cursor = self.next_cursor();
        let bytes = serde_json::to_vec(&event).expect("event").len() + 256;
        self.subscribers.retain(|s| s.queue.strong_count() > 0);
        for s in &self.subscribers {
            if matches_scope(&self.models, &s.scope, &event)
                && let Some(queue) = s.queue.upgrade()
            {
                queue.push(
                    EventEnvelope {
                        subscription_id: s.id.clone(),
                        cursor,
                        caused_by_request_id: None,
                        event: event.clone(),
                    },
                    limits,
                );
            }
        }
        self.replay_bytes += bytes;
        self.replay.push_back(Replay {
            cursor,
            event,
            bytes,
        });
        while self.replay.len() > limits.replay_events || self.replay_bytes > limits.replay_bytes {
            if let Some(old) = self.replay.pop_front() {
                self.replay_bytes -= old.bytes;
            }
        }
    }
    fn invalidate(&mut self, limits: SubscriptionLimits) {
        self.epoch = self.epoch.checked_add(1).expect("epoch exhausted");
        self.sequence = 0;
        self.replay.clear();
        self.replay_bytes = 0;
        self.snapshots.clear();
        let cursor = self.cursor();
        for s in self.subscribers.drain(..) {
            if let Some(queue) = s.queue.upgrade() {
                queue.push(
                    EventEnvelope {
                        subscription_id: s.id,
                        cursor,
                        caused_by_request_id: None,
                        event: UiEvent::ResyncRequired {
                            reason: ResyncReason::EpochChanged,
                        },
                    },
                    limits,
                );
            }
        }
    }
}
/// Transport-independent host read service. All adapters may share this instance.
pub struct Projection {
    runtime: AgentRuntime,
    registry: Arc<ApprovalRegistry>,
    state: Mutex<State>,
    pager: Pager,
    bodies: Bodies,
    limits: SubscriptionLimits,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    _commit_observer: Arc<dyn Fn() + Send + Sync>,
}
impl Projection {
    pub fn new(
        runtime: AgentRuntime,
        handle: &tokio::runtime::Handle,
        limits: SubscriptionLimits,
    ) -> Arc<Self> {
        let bus = runtime.event_bus();
        let mut events = bus.subscribe();
        let registry = ApprovalRegistry::for_bus(&bus);
        let mut approvals = registry.subscribe();
        let commits = Arc::new(tokio::sync::Notify::new());
        let wake = commits.clone();
        let observer: Arc<dyn Fn() + Send + Sync> = Arc::new(move || wake.notify_one());
        runtime.observe_event_commits(&observer);
        let this = Arc::new(Self {
            _commit_observer: observer,
            runtime,
            registry,
            pager: Pager::default(),
            bodies: Bodies::default(),
            limits,
            state: Mutex::new(State {
                #[cfg(test)]
                snapshot_hook: None,
                epoch: u64::from_be_bytes(uuid::Uuid::new_v4().as_bytes()[..8].try_into().unwrap())
                    >> 1,
                sequence: 0,
                next_subscription: 0,
                scanner: queries::CommitScanner::default(),
                initialized: false,
                projects: vec![],
                models: Models::default(),
                replay: VecDeque::new(),
                replay_bytes: 0,
                subscribers: vec![],
                snapshots: VecDeque::new(),
                answer_provenance: BTreeMap::new(),
            }),
            task: Mutex::new(None),
        });
        let weak = Arc::downgrade(&this);
        let task = handle.spawn(async move {
            loop {
                enum Change { Commit, Event(Result<event_bus::Event, RecvError>), Approval(Result<ApprovalSnapshot, tokio::sync::broadcast::error::RecvError>) }
                let change = tokio::select! { e = events.recv() => Change::Event(e), a = approvals.recv() => Change::Approval(a), _ = commits.notified() => Change::Commit };
                let Some(this) = weak.upgrade() else { break };
                match change {
                    Change::Event(Ok(event)) => {
                        // Token fragments are intentionally not projected or replayed.
                        if matches!(event.kind, EventKind::Message(event_bus::MessageEvent::MessageDelta { .. } | event_bus::MessageEvent::ReasoningDelta { .. } | event_bus::MessageEvent::ToolCallDelta { .. })) { continue; }
                        if this.refresh().is_err() { this.invalidate(); }
                    }
                    Change::Commit => { if this.refresh().is_err() { this.invalidate(); } }
                    Change::Approval(Ok(approval)) => { if this.approval_changed(&approval.id).is_err() { this.invalidate(); } }
                    Change::Event(Err(RecvError::Lagged(_))) | Change::Approval(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                        // Lost input means the projection cannot vouch for ANY old cursor.
                        this.invalidate();
                        let _ = this.refresh();
                    }
                    Change::Event(Err(RecvError::Closed)) | Change::Approval(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                }
            }
        });
        *this.task.lock().unwrap() = Some(task);
        this
    }
    pub(crate) fn configure(&self, projects: Vec<(ProjectId, PathBuf)>) {
        self.state.lock().unwrap().projects = projects;
    }
    pub fn approval_registry(&self) -> &Arc<ApprovalRegistry> {
        &self.registry
    }
    pub fn invalidate(&self) {
        self.state.lock().unwrap().invalidate(self.limits);
    }
    pub fn refresh(&self) -> Result<(), WorkbenchError> {
        let mut state = self.state.lock().unwrap();
        self.refresh_locked(&mut state)
    }
    pub(crate) fn approval_changed(
        &self,
        id: &sandbox::approval::ApprovalId,
    ) -> Result<(), WorkbenchError> {
        let mut state = self.state.lock().unwrap();
        self.refresh_after_approval(&mut state, Some(id))
    }
    fn refresh_locked(&self, state: &mut State) -> Result<(), WorkbenchError> {
        self.refresh_after_approval(state, None)
    }
    fn refresh_after_approval(
        &self,
        state: &mut State,
        observed: Option<&sandbox::approval::ApprovalId>,
    ) -> Result<(), WorkbenchError> {
        let mut next =
            queries::collect(&self.runtime, &state.projects).map_err(queries::storage_error)?;
        for (id, provenance) in &state.answer_provenance {
            if let Some(answer) = next.questions.get_mut(id).and_then(|q| q.answer.as_mut()) {
                answer.answered_by = Some(provenance.clone());
            }
        }
        let mut after = None;
        loop {
            let pending = self.registry.list_pending(after, 100);
            for a in &pending {
                if let Some(view) = queries::approval_view(a)
                    && next.threads.contains_key(view.thread.as_str())
                {
                    next.approvals.insert(view.id.as_str().to_owned(), view);
                }
            }
            if pending.len() < 100 {
                break;
            }
            after = pending.last().map(|a| a.attempt);
        }
        // A request can be registered and decided before a refresh sees Pending.
        // Re-read the notified identity from the canonical registry, not a stale
        // payload; retain terminal decisions for subsequent diffs as well.
        for id in state
            .models
            .approvals
            .values()
            .map(|old| sandbox::approval::ApprovalId {
                core_epoch: old.core_epoch.get(),
                value: old.id.as_str().to_owned(),
            })
            .chain(observed.cloned())
        {
            if let Some(a) = self
                .registry
                .get(&id)
                .and_then(|a| queries::approval_view(&a))
                && next.threads.contains_key(a.thread.as_str())
            {
                next.approvals.insert(a.id.as_str().to_owned(), a);
            }
        }
        for run in next.runs.values_mut() {
            if next
                .approvals
                .values()
                .any(|a| a.run == run.id && a.state == ApprovalState::Pending)
            {
                run.phase = RunPhase::WaitingForApproval;
            }
            run.revision = Revision(0);
            run.revision = queries::revision(&run);
        }
        for thread in next.threads.values_mut() {
            thread.pending_approvals = next
                .approvals
                .values()
                .filter(|a| a.thread == thread.summary.id && a.state == ApprovalState::Pending)
                .count() as u32;
            if thread.pending_approvals > 0 {
                thread.summary.state = ThreadState::WaitingForApproval;
            }
            thread.summary.revision = Revision(0);
            thread.summary.revision = queries::revision(&thread.summary);
        }
        let mut changes = Vec::new();
        for (id, value) in &next.threads {
            if state.models.threads.get(id) != Some(value) {
                changes.push(UiEvent::ThreadChanged {
                    thread: value.summary.clone(),
                });
            }
        }
        for (id, value) in &next.runs {
            if state.models.runs.get(id) != Some(value) {
                changes.push(UiEvent::RunChanged { run: value.clone() });
            }
        }
        for (id, value) in &next.questions {
            if state.models.questions.get(id) != Some(value) {
                changes.push(UiEvent::QuestionChanged {
                    question: value.clone(),
                });
            }
        }
        for (id, value) in &next.approvals {
            if state.models.approvals.get(id) != Some(value) {
                changes.push(UiEvent::ApprovalChanged {
                    approval: value.clone(),
                });
            }
        }
        for (id, value) in &next.receipts {
            if state.models.receipts.get(id) != Some(value) {
                changes.push(UiEvent::InputReceiptChanged {
                    receipt: value.clone(),
                });
            }
        }
        if state.models.status() != next.status() {
            changes.push(UiEvent::CoreStatusChanged {
                status: next.status(),
            });
        }
        // An old snapshot/replay/queued DTO must never resurrect a deleted
        // subject, or retain a question after its routing scope changed.
        let removed = state.models.threads.iter().any(|(id, old)| {
            next.threads
                .get(id)
                .is_none_or(|new| new.summary.project != old.summary.project)
        }) || state
            .models
            .runs
            .keys()
            .any(|id| !next.runs.contains_key(id))
            || state.models.questions.iter().any(|(id, old)| {
                next.questions
                    .get(id)
                    .is_none_or(|new| new.target != old.target)
            })
            || state
                .models
                .approvals
                .keys()
                .any(|id| !next.approvals.contains_key(id))
            || state
                .models
                .receipts
                .keys()
                .any(|id| !next.receipts.contains_key(id));
        state.models = next;
        if removed {
            state.invalidate(self.limits);
        }
        for event in changes {
            state.publish(event, self.limits);
        }
        let mut scanner = std::mem::take(&mut state.scanner);
        let runs: Vec<_> = state.models.runs.keys().cloned().collect();
        let initialized = state.initialized;
        let result = self
            .bodies
            .changes(&self.runtime, &mut scanner, &runs, |message| {
                if initialized && state.models.threads.contains_key(message.thread.as_str()) {
                    state.publish(UiEvent::MessageCommitted { message }, self.limits);
                }
            });
        state.scanner = scanner;
        if !result.as_ref().is_ok_and(|rewritten| !*rewritten) {
            state.invalidate(self.limits);
        }
        result?;
        state.initialized = true;
        Ok(())
    }
    pub(crate) fn note_answer(&self, id: &str, provenance: &Provenance) {
        let mut state = self.state.lock().unwrap();
        // Bounded bookkeeping, not prompt content.
        if state.answer_provenance.len() < 4096 {
            state
                .answer_provenance
                .entry(id.to_owned())
                .or_insert_with(|| provenance.clone());
        }
    }
    pub fn query(&self, query: &Query) -> Result<QueryReply, WorkbenchError> {
        query.validate(&Default::default())?;
        if let Query::ReadBody(q) = query {
            return Ok(QueryReply::Body(self.bodies.read(&self.runtime, q)?));
        }
        if let Query::ReadCommittedHistory(q) = query {
            return Ok(QueryReply::CommittedHistory(self.bodies.history(
                &self.runtime,
                &self.pager,
                q,
            )?));
        }
        let mut state = self.state.lock().unwrap();
        self.refresh_locked(&mut state)?;
        let m = &state.models;
        macro_rules! page {
            ($variant:ident, $values:expr, $request:expr) => {{
                let values: Vec<_> = $values;
                let mut key = query.clone();
                // The cursor is excluded from the query identity.
                match &mut key {
                    Query::ListProjects(p) => p.cursor = None,
                    Query::ListThreads(q) => q.page.cursor = None,
                    Query::ListRuns(q) | Query::GetInputReceipts(q) => q.page.cursor = None,
                    Query::ListQuestions(q) | Query::ListApprovals(q) => q.page.cursor = None,
                    _ => {}
                }
                QueryReply::$variant(self.pager.page(
                    &serde_json::to_string(&key).unwrap(),
                    &values,
                    $request,
                    queries::revision(&values),
                )?)
            }};
        }
        Ok(match query {
            Query::GetCoreStatus => QueryReply::CoreStatus(m.status()),
            Query::ListProjects(p) => page!(Projects, m.projects.values().cloned().collect(), p),
            Query::ListThreads(q) => page!(
                Threads,
                m.threads
                    .values()
                    .filter(|t| t.summary.project == q.project)
                    .map(|t| t.summary.clone())
                    .collect(),
                &q.page
            ),
            Query::GetThread(q) => QueryReply::Thread(
                m.threads
                    .get(q.thread.as_str())
                    .cloned()
                    .ok_or_else(|| queries::error(ErrorCode::NotFound, "unknown thread"))?,
            ),
            Query::ListRuns(q) => page!(
                Runs,
                m.runs
                    .values()
                    .filter(|r| r.thread == q.thread)
                    .cloned()
                    .collect(),
                &q.page
            ),
            Query::ListQuestions(q) => page!(
                Questions,
                m.questions
                    .values()
                    .filter(
                        |v| q.thread.as_ref().is_none_or(|t| *t == v.requester.thread)
                            && (!q.pending_only || v.answer.is_none())
                    )
                    .cloned()
                    .collect(),
                &q.page
            ),
            Query::ListApprovals(q) => page!(
                Approvals,
                m.approvals
                    .values()
                    .filter(|v| q.thread.as_ref().is_none_or(|t| *t == v.thread)
                        && (!q.pending_only || v.state == ApprovalState::Pending))
                    .cloned()
                    .collect(),
                &q.page
            ),
            Query::GetInputReceipts(q) => page!(
                InputReceipts,
                m.receipts
                    .values()
                    .filter(|r| r.thread == q.thread)
                    .cloned()
                    .collect(),
                &q.page
            ),
            _ => {
                return Err(queries::error(
                    ErrorCode::UnsupportedCapability,
                    "command receipt belongs to its ingress session",
                ));
            }
        })
    }
    pub fn subscribe_with_snapshot(
        &self,
        request: &Subscribe,
    ) -> Result<Subscription, WorkbenchError> {
        let mut state = self.state.lock().unwrap();
        self.refresh_locked(&mut state)?;
        match &request.scope {
            SubscriptionScope::Project { project }
                if !state.models.projects.contains_key(project.as_str()) =>
            {
                return Err(queries::error(ErrorCode::NotFound, "unknown project"));
            }
            SubscriptionScope::Thread { thread }
                if !state.models.threads.contains_key(thread.as_str()) =>
            {
                return Err(queries::error(ErrorCode::NotFound, "unknown thread"));
            }
            _ => {}
        }
        state.subscribers.retain(|s| s.queue.strong_count() > 0);
        if state.subscribers.len() >= 1024 {
            return Err(queries::error(ErrorCode::Busy, "too many subscriptions"));
        }
        state.next_subscription = state
            .next_subscription
            .checked_add(1)
            .ok_or_else(|| queries::error(ErrorCode::Busy, "subscription ids exhausted"))?;
        let id = SubscriptionId::new(state.next_subscription.to_string()).unwrap();
        let cursor = state.cursor();
        let replayable = request.after.is_some_and(|after| {
            after.core_epoch == cursor.core_epoch
                && after.sequence <= cursor.sequence
                && state
                    .replay
                    .front()
                    .map_or(after.sequence == cursor.sequence, |first| {
                        after.sequence.get().saturating_add(1) >= first.cursor.sequence.get()
                    })
        });
        let (snapshot_id, initial_page) = if replayable {
            (None, None)
        } else {
            let id = SnapshotId::new(uuid::Uuid::new_v4().to_string()).unwrap();
            #[cfg(test)]
            if let Some(hook) = state.snapshot_hook.take() {
                hook();
            }
            let items = snapshot_items(&state.models, &request.scope);
            if serde_json::to_vec(&items).unwrap().len() > 8 * 1024 * 1024 {
                return Err(queries::error(
                    ErrorCode::Busy,
                    "snapshot exceeds retention budget; use a narrower scope",
                ));
            }
            let revision = Revision(cursor.sequence.get());
            let page = self
                .pager
                .page(id.as_str(), &items, &PageRequest::default(), revision)?;
            state.snapshots.push_back(Frozen {
                id: id.clone(),
                items,
                revision,
            });
            while state.snapshots.len() > 4 {
                state.snapshots.pop_front();
            }
            (Some(id), Some(page))
        };
        let started = SubscriptionStarted {
            subscription_id: id,
            snapshot_id,
            high_water: if replayable {
                request.after.unwrap()
            } else {
                cursor
            },
            initial_page,
        };
        let (subscription, subscriber) = subscriptions::attach(started, request.scope.clone());
        if replayable {
            for entry in state
                .replay
                .iter()
                .filter(|entry| entry.cursor.sequence > request.after.unwrap().sequence)
            {
                if matches_scope(&state.models, &request.scope, &entry.event) {
                    subscription.queue.push(
                        EventEnvelope {
                            subscription_id: subscriber.id.clone(),
                            cursor: entry.cursor,
                            caused_by_request_id: None,
                            event: entry.event.clone(),
                        },
                        self.limits,
                    );
                }
            }
        }
        // A fallback snapshot explicitly replaces stale client state. Signal the
        // rejected cursor on a terminal stream; the caller must resubscribe.
        if let Some(after) = request.after.filter(|_| !replayable) {
            let terminal_cursor = state.next_cursor();
            subscription.queue.push(
                EventEnvelope {
                    subscription_id: subscriber.id.clone(),
                    cursor: terminal_cursor,
                    caused_by_request_id: None,
                    event: UiEvent::ResyncRequired {
                        reason: if after.core_epoch != cursor.core_epoch {
                            ResyncReason::EpochChanged
                        } else {
                            ResyncReason::CursorTooOld
                        },
                    },
                },
                self.limits,
            );
        } else {
            state.subscribers.push(subscriber);
        }
        Ok(subscription)
    }
    pub fn snapshot_page(
        &self,
        snapshot: &SnapshotId,
        page: &PageRequest,
    ) -> Result<Page<SnapshotItem>, WorkbenchError> {
        let mut state = self.state.lock().unwrap();
        self.refresh_locked(&mut state)?;
        let snapshot = state
            .snapshots
            .iter()
            .find(|s| &s.id == snapshot)
            .ok_or_else(|| queries::error(ErrorCode::Conflict, "snapshot expired"))?;
        self.pager.page(
            snapshot.id.as_str(),
            &snapshot.items,
            page,
            snapshot.revision,
        )
    }
    pub fn unsubscribe(&self, id: &SubscriptionId) {
        let mut state = self.state.lock().unwrap();
        if let Some(index) = state.subscribers.iter().position(|s| &s.id == id) {
            let s = state.subscribers.remove(index);
            if let Some(queue) = s.queue.upgrade() {
                let cursor = state.next_cursor();
                queue.push(
                    EventEnvelope {
                        subscription_id: s.id,
                        cursor,
                        caused_by_request_id: None,
                        event: UiEvent::SubscriptionClosed {
                            reason: CloseReason::Unsubscribed,
                        },
                    },
                    self.limits,
                );
            }
        }
    }
}
impl Drop for Projection {
    fn drop(&mut self) {
        if let Some(task) = self.task.get_mut().unwrap().take() {
            task.abort();
        }
        let state = self.state.get_mut().unwrap();
        let cursor = state.next_cursor();
        for subscriber in state.subscribers.drain(..) {
            if let Some(queue) = subscriber.queue.upgrade() {
                queue.push(
                    EventEnvelope {
                        subscription_id: subscriber.id,
                        cursor,
                        caused_by_request_id: None,
                        event: UiEvent::SubscriptionClosed {
                            reason: CloseReason::CoreShuttingDown,
                        },
                    },
                    self.limits,
                );
            }
        }
    }
}
fn thread_of(event: &UiEvent) -> Option<&ThreadId> {
    match event {
        UiEvent::ThreadChanged { thread } => Some(&thread.id),
        UiEvent::RunChanged { run } => Some(&run.thread),
        UiEvent::QuestionChanged { question } => Some(&question.requester.thread),
        UiEvent::ApprovalChanged { approval } => Some(&approval.thread),
        UiEvent::InputReceiptChanged { receipt } => Some(&receipt.thread),
        UiEvent::MessageCommitted { message } => Some(&message.thread),
        _ => None,
    }
}
fn matches_scope(models: &Models, scope: &SubscriptionScope, event: &UiEvent) -> bool {
    if event.ends_subscription() || matches!(event, UiEvent::CoreStatusChanged { .. }) {
        return true;
    }
    let routed = match event {
        UiEvent::QuestionChanged { question } => match &question.target {
            QuestionTarget::Lead { lead_thread, .. } => Some(lead_thread),
            _ => None,
        },
        _ => None,
    };
    let threads = [thread_of(event), routed];
    match scope {
        SubscriptionScope::All => true,
        SubscriptionScope::Thread { thread } => threads.contains(&Some(thread)),
        SubscriptionScope::Project { project } => threads.into_iter().flatten().any(|t| {
            models
                .threads
                .get(t.as_str())
                .is_some_and(|t| &t.summary.project == project)
        }),
    }
}
fn snapshot_items(models: &Models, scope: &SubscriptionScope) -> Vec<SnapshotItem> {
    let mut items = vec![SnapshotItem::CoreStatus(models.status())];
    items.extend(
        models
            .projects
            .values()
            .filter(|p| match scope {
                SubscriptionScope::All => true,
                SubscriptionScope::Project { project } => &p.id == project,
                SubscriptionScope::Thread { thread } => models
                    .threads
                    .get(thread.as_str())
                    .is_some_and(|t| t.summary.project == p.id),
            })
            .cloned()
            .map(SnapshotItem::Project),
    );
    items.extend(
        models
            .threads
            .values()
            .filter(|t| {
                matches_scope(
                    models,
                    scope,
                    &UiEvent::ThreadChanged {
                        thread: t.summary.clone(),
                    },
                )
            })
            .map(|t| SnapshotItem::Thread(t.summary.clone())),
    );
    items.extend(
        models
            .runs
            .values()
            .filter(|r| matches_scope(models, scope, &UiEvent::RunChanged { run: (*r).clone() }))
            .cloned()
            .map(SnapshotItem::Run),
    );
    items.extend(
        models
            .questions
            .values()
            .filter(|q| {
                q.answer.is_none()
                    && matches_scope(
                        models,
                        scope,
                        &UiEvent::QuestionChanged {
                            question: (*q).clone(),
                        },
                    )
            })
            .cloned()
            .map(SnapshotItem::Question),
    );
    items.extend(
        models
            .approvals
            .values()
            .filter(|a| {
                a.state == ApprovalState::Pending
                    && matches_scope(
                        models,
                        scope,
                        &UiEvent::ApprovalChanged {
                            approval: (*a).clone(),
                        },
                    )
            })
            .cloned()
            .map(SnapshotItem::Approval),
    );
    items
}

#[cfg(test)]
#[path = "projection_tests.rs"]
mod tests;
