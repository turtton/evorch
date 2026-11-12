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
use workbench_protocol::namespaces::{
    ApprovalsEvent, ApprovalsQuery, ApprovalsReply, ConversationEvent, ConversationQuery,
    ConversationReply, ExecutionEvent, ExecutionQuery, ExecutionReply, LifecycleEvent,
    LifecycleQuery, LifecycleReply, ProjectionEvent, ProjectionQuery, ProjectionReply,
    QuestionsEvent, QuestionsQuery, QuestionsReply, WorkspaceEvent, WorkspaceQuery, WorkspaceReply,
};
use workbench_protocol::{
    envelope::Provenance,
    errors::{ErrorCode, WorkbenchError},
    events::*,
    ids::*,
    queries::{PageRequest, Query, QueryReply, ReadSnapshot, SnapshotPage},
    views::*,
    wire::*,
};

struct Replay {
    cursor: EventCursor,
    event: UiEvent,
    bytes: usize,
}
/// Frozen cuts survive connection closure, but not epoch invalidation.
const SNAPSHOT_TTL: std::time::Duration = std::time::Duration::from_secs(5 * 60);
const MAX_SNAPSHOTS: usize = 4;
struct Frozen {
    id: SnapshotId,
    items: Vec<SnapshotItem>,
    revision: Revision,
    high_water: EventCursor,
    created_at: tokio::time::Instant,
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
    fn expire_snapshots(&mut self) {
        let now = tokio::time::Instant::now();
        self.snapshots
            .retain(|s| now.saturating_duration_since(s.created_at) < SNAPSHOT_TTL);
    }
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
                        event: UiEvent::Projection(ProjectionEvent::ResyncRequired {
                            reason: ResyncReason::EpochChanged,
                        }),
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
    /// Epoch used by the handshake and subscription cursors.
    pub fn instance_epoch(&self) -> InstanceEpoch {
        InstanceEpoch(self.state.lock().unwrap().epoch)
    }

    pub fn approval_registry(&self) -> &Arc<ApprovalRegistry> {
        &self.registry
    }

    /// A [`ProjectionSource`] for the events of `namespace`. The `projection`
    /// namespace is the projection's own control channel and has no source.
    ///
    /// # Errors
    /// `InvalidRequest` for the `projection` namespace or a name that is no
    /// namespace.
    pub fn source(self: &Arc<Self>, namespace: &str) -> Result<ProjectionSource, WorkbenchError> {
        let info = workbench_protocol::namespaces::namespace(namespace)
            .filter(|info| info.name != "projection")
            .ok_or_else(|| {
                queries::error(
                    ErrorCode::InvalidRequest,
                    "no event source for this namespace",
                )
            })?;
        Ok(ProjectionSource {
            projection: Arc::downgrade(self),
            namespace: info.name,
        })
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
                changes.push(UiEvent::Workspace(WorkspaceEvent::ThreadChanged {
                    thread: value.summary.clone(),
                }));
            }
        }
        for (id, value) in &next.runs {
            if state.models.runs.get(id) != Some(value) {
                changes.push(UiEvent::Execution(ExecutionEvent::RunChanged {
                    run: value.clone(),
                }));
            }
        }
        for (id, value) in &next.questions {
            if state.models.questions.get(id) != Some(value) {
                changes.push(UiEvent::Questions(QuestionsEvent::QuestionChanged {
                    question: value.clone(),
                }));
            }
        }
        for (id, value) in &next.approvals {
            if state.models.approvals.get(id) != Some(value) {
                changes.push(UiEvent::Approvals(ApprovalsEvent::ApprovalChanged {
                    approval: value.clone(),
                }));
            }
        }
        for (id, value) in &next.receipts {
            if state.models.receipts.get(id) != Some(value) {
                changes.push(UiEvent::Conversation(
                    ConversationEvent::InputReceiptChanged {
                        receipt: value.clone(),
                    },
                ));
            }
        }
        if state.models.status() != next.status() {
            changes.push(UiEvent::Lifecycle(LifecycleEvent::CoreStatusChanged {
                status: next.status(),
            }));
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
                    state.publish(
                        UiEvent::Conversation(ConversationEvent::MessageCommitted { message }),
                        self.limits,
                    );
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
    /// Read-only queries the projection answers. Each namespace has its own
    /// reader; a namespace this projection does not serve (the host answers
    /// lane management, command receipts and approval inputs itself, and the
    /// other namespaces through their area services) is refused.
    pub fn query(&self, query: &Query) -> Result<QueryReply, WorkbenchError> {
        query.validate(&Default::default())?;
        match query {
            Query::Conversation(q) => self
                .conversation_query(query, q)
                .map(QueryReply::Conversation),
            Query::Workspace(q) => self.workspace_query(query, q).map(QueryReply::Workspace),
            Query::Execution(q) => self.execution_query(query, q).map(QueryReply::Execution),
            Query::Questions(q) => self.questions_query(query, q).map(QueryReply::Questions),
            Query::Approvals(q) => self.approvals_query(query, q).map(QueryReply::Approvals),
            Query::Lifecycle(q) => self.lifecycle_query(q).map(QueryReply::Lifecycle),
            Query::Projection(q) => self.projection_query(q).map(QueryReply::Projection),
            // Every other namespace is answered by the host's area service for
            // it (lane management, MCP, teams, ...), never by this projection;
            // a namespace gaining operations does not change this match.
            Query::Lanes(_)
            | Query::Settings(_)
            | Query::Auth(_)
            | Query::Mcp(_)
            | Query::Teams(_)
            | Query::Diagnostics(_)
            | Query::Memory(_)
            | Query::Evaluation(_)
            | Query::Improvement(_)
            | Query::PullRequests(_)
            | Query::Browser(_)
            | Query::Artifacts(_)
            | Query::Maintenance(_) => Err(area_owned()),
        }
    }

    /// Run `read` on the models after bringing them up to date.
    fn with_models<T>(
        &self,
        read: impl FnOnce(&Models) -> Result<T, WorkbenchError>,
    ) -> Result<T, WorkbenchError> {
        let mut state = self.state.lock().unwrap();
        self.refresh_locked(&mut state)?;
        read(&state.models)
    }

    /// One page of `values`, identified by `query` with its cursor left out.
    fn page_of<T: Clone + serde::Serialize>(
        &self,
        query: &Query,
        values: Vec<T>,
        request: &PageRequest,
    ) -> Result<Page<T>, WorkbenchError> {
        let mut key = query.clone();
        match &mut key {
            Query::Workspace(WorkspaceQuery::ListProjects(p)) => p.cursor = None,
            Query::Workspace(WorkspaceQuery::ListThreads(q)) => q.page.cursor = None,
            Query::Execution(ExecutionQuery::ListRuns(q))
            | Query::Conversation(ConversationQuery::GetInputReceipts(q)) => q.page.cursor = None,
            Query::Questions(QuestionsQuery::ListQuestions(q))
            | Query::Approvals(ApprovalsQuery::ListApprovals(q)) => q.page.cursor = None,
            _ => {}
        }
        self.pager.page(
            &serde_json::to_string(&key).unwrap(),
            &values,
            request,
            queries::revision(&values),
        )
    }

    fn conversation_query(
        &self,
        query: &Query,
        q: &ConversationQuery,
    ) -> Result<ConversationReply, WorkbenchError> {
        match q {
            ConversationQuery::ReadCommittedHistory(q) => Ok(ConversationReply::CommittedHistory(
                self.bodies.history(&self.runtime, &self.pager, q)?,
            )),
            ConversationQuery::GetInputReceipts(q) => self.with_models(|m| {
                Ok(ConversationReply::InputReceipts(
                    self.page_of(
                        query,
                        m.receipts
                            .values()
                            .filter(|r| r.thread == q.thread)
                            .cloned()
                            .collect(),
                        &q.page,
                    )?,
                ))
            }),
        }
    }

    // Operations the workspace area adds later are answered by its host service
    // and never routed here; the fallthrough keeps this reader compiling when
    // the namespace gains a variant.
    #[allow(unreachable_patterns)]
    fn workspace_query(
        &self,
        query: &Query,
        q: &WorkspaceQuery,
    ) -> Result<WorkspaceReply, WorkbenchError> {
        self.with_models(|m| {
            Ok(match q {
                WorkspaceQuery::ListProjects(p) => WorkspaceReply::Projects(self.page_of(
                    query,
                    m.projects.values().cloned().collect(),
                    p,
                )?),
                WorkspaceQuery::ListThreads(q) => WorkspaceReply::Threads(
                    self.page_of(
                        query,
                        m.threads
                            .values()
                            .filter(|t| t.summary.project == q.project)
                            .map(|t| t.summary.clone())
                            .collect(),
                        &q.page,
                    )?,
                ),
                WorkspaceQuery::GetThread(q) => WorkspaceReply::Thread(
                    m.threads
                        .get(q.thread.as_str())
                        .cloned()
                        .ok_or_else(|| queries::error(ErrorCode::NotFound, "unknown thread"))?,
                ),
                _ => return Err(area_owned()),
            })
        })
    }

    // As for `workspace_query`: the execution area's later operations belong to
    // its host service.
    #[allow(irrefutable_let_patterns)]
    fn execution_query(
        &self,
        query: &Query,
        q: &ExecutionQuery,
    ) -> Result<ExecutionReply, WorkbenchError> {
        let ExecutionQuery::ListRuns(q) = q else {
            return Err(area_owned());
        };
        self.with_models(|m| {
            Ok(ExecutionReply::Runs(
                self.page_of(
                    query,
                    m.runs
                        .values()
                        .filter(|r| r.thread == q.thread)
                        .cloned()
                        .collect(),
                    &q.page,
                )?,
            ))
        })
    }

    fn questions_query(
        &self,
        query: &Query,
        q: &QuestionsQuery,
    ) -> Result<QuestionsReply, WorkbenchError> {
        let QuestionsQuery::ListQuestions(q) = q;
        self.with_models(|m| {
            Ok(QuestionsReply::Questions(
                self.page_of(
                    query,
                    m.questions
                        .values()
                        .filter(|v| {
                            q.thread.as_ref().is_none_or(|t| *t == v.requester.thread)
                                && (!q.pending_only || v.answer.is_none())
                        })
                        .cloned()
                        .collect(),
                    &q.page,
                )?,
            ))
        })
    }

    /// The approval input is read by the host, which owns the registry check.
    fn approvals_query(
        &self,
        query: &Query,
        q: &ApprovalsQuery,
    ) -> Result<ApprovalsReply, WorkbenchError> {
        match q {
            ApprovalsQuery::ListApprovals(q) => self.with_models(|m| {
                Ok(ApprovalsReply::Approvals(
                    self.page_of(
                        query,
                        m.approvals
                            .values()
                            .filter(|v| {
                                q.thread.as_ref().is_none_or(|t| *t == v.thread)
                                    && (!q.pending_only || v.state == ApprovalState::Pending)
                            })
                            .cloned()
                            .collect(),
                        &q.page,
                    )?,
                ))
            }),
            ApprovalsQuery::GetApprovalInput(_) => Err(queries::error(
                ErrorCode::UnsupportedCapability,
                "approval input belongs to the host ingress",
            )),
        }
    }

    fn lifecycle_query(&self, q: &LifecycleQuery) -> Result<LifecycleReply, WorkbenchError> {
        let LifecycleQuery::GetCoreStatus = q;
        self.with_models(|m| Ok(LifecycleReply::CoreStatus(m.status())))
    }

    fn projection_query(&self, q: &ProjectionQuery) -> Result<ProjectionReply, WorkbenchError> {
        match q {
            ProjectionQuery::ReadSnapshot(q) => {
                Ok(ProjectionReply::Snapshot(self.read_snapshot(q)?))
            }
            ProjectionQuery::ReadBody(q) => {
                Ok(ProjectionReply::Body(self.bodies.read(&self.runtime, q)?))
            }
            ProjectionQuery::GetCommandReceipt { .. } => Err(queries::error(
                ErrorCode::UnsupportedCapability,
                "command receipt belongs to its ingress session",
            )),
        }
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
        state.expire_snapshots();
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
                high_water: cursor,
                created_at: tokio::time::Instant::now(),
            });
            while state.snapshots.len() > MAX_SNAPSHOTS {
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
                    event: UiEvent::Projection(ProjectionEvent::ResyncRequired {
                        reason: if after.core_epoch != cursor.core_epoch {
                            ResyncReason::EpochChanged
                        } else {
                            ResyncReason::CursorTooOld
                        },
                    }),
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
        Ok(self
            .read_snapshot(&ReadSnapshot {
                snapshot_id: snapshot.clone(),
                page: page.clone(),
            })?
            .page)
    }
    pub fn read_snapshot(&self, request: &ReadSnapshot) -> Result<SnapshotPage, WorkbenchError> {
        request.page.validate()?;
        let mut state = self.state.lock().unwrap();
        self.refresh_locked(&mut state)?;
        state.expire_snapshots();
        let snapshot = state
            .snapshots
            .iter()
            .find(|s| s.id == request.snapshot_id)
            .ok_or_else(|| {
                queries::error(
                    ErrorCode::ResyncRequired,
                    "snapshot expired or its epoch changed",
                )
            })?;
        let page = self
            .pager
            .page(
                snapshot.id.as_str(),
                &snapshot.items,
                &request.page,
                snapshot.revision,
            )
            .map_err(|e| {
                if e.code == ErrorCode::Conflict {
                    queries::error(
                        ErrorCode::ResyncRequired,
                        "snapshot cursor expired or belongs to another cut",
                    )
                } else {
                    e
                }
            })?;
        Ok(SnapshotPage {
            snapshot_id: snapshot.id.clone(),
            high_water: snapshot.high_water,
            page,
        })
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
                        event: UiEvent::Projection(ProjectionEvent::SubscriptionClosed {
                            reason: CloseReason::Unsubscribed,
                        }),
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
                        event: UiEvent::Projection(ProjectionEvent::SubscriptionClosed {
                            reason: CloseReason::CoreShuttingDown,
                        }),
                    },
                    self.limits,
                );
            }
        }
    }
}
fn area_owned() -> WorkbenchError {
    queries::error(
        ErrorCode::UnsupportedCapability,
        "this operation belongs to its area service",
    )
}

/// How an area service feeds events of its own namespace into the single
/// publication boundary of the projection: they get the next cursor, enter the
/// replay buffer and reach every subscriber like any other event. Obtain one
/// with [`Projection::source`]; it holds the projection weakly, so a source kept
/// by a service never keeps the projection alive.
///
/// An event that names no thread (everything an area adds, until a namespace
/// defines how its events map to threads) reaches `All`-scope subscribers only.
#[derive(Clone)]
pub struct ProjectionSource {
    projection: std::sync::Weak<Projection>,
    namespace: &'static str,
}

impl ProjectionSource {
    pub fn namespace(&self) -> &'static str {
        self.namespace
    }

    /// Publish `event`, which must belong to this source's namespace.
    ///
    /// # Errors
    /// `InvalidRequest` when the event fails its own validation or belongs to
    /// another namespace, `Draining` when the projection is gone.
    pub fn publish(&self, event: UiEvent) -> Result<(), WorkbenchError> {
        if event.namespace() != self.namespace {
            return Err(queries::error(
                ErrorCode::InvalidRequest,
                "event belongs to another namespace",
            ));
        }
        event.validate()?;
        let projection = self.projection.upgrade().ok_or_else(|| {
            queries::error(ErrorCode::Draining, "the projection is shutting down")
        })?;
        projection
            .state
            .lock()
            .unwrap()
            .publish(event, projection.limits);
        Ok(())
    }
}

fn thread_of(event: &UiEvent) -> Option<&ThreadId> {
    match event {
        UiEvent::Workspace(WorkspaceEvent::ThreadChanged { thread }) => Some(&thread.id),
        UiEvent::Execution(ExecutionEvent::RunChanged { run }) => Some(&run.thread),
        UiEvent::Questions(QuestionsEvent::QuestionChanged { question }) => {
            Some(&question.requester.thread)
        }
        UiEvent::Approvals(ApprovalsEvent::ApprovalChanged { approval }) => Some(&approval.thread),
        UiEvent::Conversation(ConversationEvent::InputReceiptChanged { receipt }) => {
            Some(&receipt.thread)
        }
        UiEvent::Conversation(ConversationEvent::MessageCommitted { message }) => {
            Some(&message.thread)
        }
        _ => None,
    }
}
fn matches_scope(models: &Models, scope: &SubscriptionScope, event: &UiEvent) -> bool {
    if event.ends_subscription()
        || matches!(
            event,
            UiEvent::Lifecycle(LifecycleEvent::CoreStatusChanged { .. })
        )
    {
        return true;
    }
    let routed = match event {
        UiEvent::Questions(QuestionsEvent::QuestionChanged { question }) => {
            match &question.target {
                QuestionTarget::Lead { lead_thread, .. } => Some(lead_thread),
                _ => None,
            }
        }
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
                    &UiEvent::Workspace(WorkspaceEvent::ThreadChanged {
                        thread: t.summary.clone(),
                    }),
                )
            })
            .map(|t| SnapshotItem::Thread(t.summary.clone())),
    );
    items.extend(
        models
            .runs
            .values()
            .filter(|r| {
                matches_scope(
                    models,
                    scope,
                    &UiEvent::Execution(ExecutionEvent::RunChanged { run: (*r).clone() }),
                )
            })
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
                        &UiEvent::Questions(QuestionsEvent::QuestionChanged {
                            question: (*q).clone(),
                        }),
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
                        &UiEvent::Approvals(ApprovalsEvent::ApprovalChanged {
                            approval: (*a).clone(),
                        }),
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
