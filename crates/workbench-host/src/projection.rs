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
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
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
    handshake::CoreStatus,
    ids::*,
    queries::{PageRequest, Query, QueryReply, ReadSnapshot, SnapshotPage},
    views::*,
    wire::*,
};

mod activity;
mod shell_jobs;

struct Replay {
    cursor: EventCursor,
    event: UiEvent,
    bytes: usize,
}
/// Frozen cuts survive connection closure, but not epoch invalidation.
const SNAPSHOT_TTL: std::time::Duration = std::time::Duration::from_secs(5 * 60);
const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
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
    #[cfg(test)]
    refresh_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    epoch: u64,
    epoch_tx: Arc<tokio::sync::watch::Sender<u64>>,
    sequence: u64,
    next_subscription: u64,
    models_ready: bool,
    projects: Vec<(ProjectId, PathBuf)>,
    /// The names saved with the projects; an id stands in for a missing one.
    project_names: std::collections::BTreeMap<String, String>,
    models: Models,
    activities: BTreeMap<(String, String), activity::LiveTool>,
    shell_jobs: shell_jobs::ShellJobs,
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
        self.epoch_tx.send_replace(self.epoch);
        self.sequence = 0;
        self.activities.clear();
        self.shell_jobs.clear();
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
#[derive(Default)]
struct BodyCursor {
    #[cfg(test)]
    advance_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    scanner: queries::CommitScanner,
    reader: Option<storage::Database>,
}
#[derive(Default)]
struct ModelRefresh {
    reader: Option<storage::Database>,
}
/// Transport-independent host read service. All adapters may share this instance.
pub struct Projection {
    activity_now: Arc<dyn Fn() -> std::time::Duration + Send + Sync>,
    activity_redactor: secret_guard::SecretRedactor,
    #[cfg(test)]
    activity_formats: std::sync::atomic::AtomicUsize,
    runtime: AgentRuntime,
    registry: Arc<ApprovalRegistry>,
    state: Mutex<State>,
    /// Lock order: model_refresh -> state. Serializes collect through publish,
    /// including project reconfiguration, without blocking readers of State.
    model_refresh: Mutex<ModelRefresh>,
    #[cfg(test)]
    model_refreshes: tokio::sync::watch::Sender<usize>,
    #[cfg(test)]
    model_event_batches: tokio::sync::watch::Sender<usize>,
    epoch: Arc<tokio::sync::watch::Sender<u64>>,
    status: Mutex<CoreStatus>,
    pager: Pager,
    bodies: Bodies,
    body_cursor: Mutex<BodyCursor>,
    bodies_wake: Arc<tokio::sync::Notify>,
    cut_taken: AtomicBool,
    limits: SubscriptionLimits,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    _commit_observer: Arc<dyn Fn() + Send + Sync>,
}
// Bound work per wakeup, not time: a continuously busy producer must still let runs advance.
const MAX_REFRESH_WAKEUPS: usize = 64;
const MIN_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Default, Debug, PartialEq)]
struct RefreshWakeups {
    dirty: bool,
    lagged: bool,
    closed: bool,
    activities: Vec<event_bus::Event>,
}

impl RefreshWakeups {
    async fn drain(
        events: &mut event_bus::EventReceiver,
        mut event: Result<event_bus::Event, RecvError>,
    ) -> Self {
        let mut wakeups = Self::default();
        for index in 0..MAX_REFRESH_WAKEUPS {
            match event {
                Ok(event) => {
                    // Fragments are neither projected nor replayed.
                    wakeups.dirty |= !matches!(
                        event.kind,
                        EventKind::Tool(event_bus::ToolEvent::ShellJobOutput { .. })
                            | EventKind::Message(
                                event_bus::MessageEvent::MessageDelta { .. }
                                    | event_bus::MessageEvent::ReasoningDelta { .. }
                                    | event_bus::MessageEvent::ToolCallDelta { .. }
                            )
                    );
                    if activity::relevant(&event.kind)
                        || matches!(
                            event.kind,
                            EventKind::Tool(event_bus::ToolEvent::ShellJobOutput { .. })
                        )
                    {
                        wakeups.activities.push(event);
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    wakeups.activities.clear();
                    wakeups.lagged = true;
                    wakeups.dirty = true;
                }
                Err(RecvError::Closed) => {
                    wakeups.closed = true;
                    break;
                }
            }
            if index + 1 == MAX_REFRESH_WAKEUPS {
                break;
            }
            // recv is cancellation-safe. Never wait for another event to form a batch;
            // also stop if recv yields cooperatively with more events still queued.
            event = tokio::select! {
                biased;
                event = events.recv() => event,
                () = std::future::ready(()) => break,
            };
        }
        wakeups
    }
}

impl Projection {
    pub fn new(
        runtime: AgentRuntime,
        handle: &tokio::runtime::Handle,
        limits: SubscriptionLimits,
    ) -> Arc<Self> {
        Self::with_epoch(runtime, handle, limits, None)
    }
    /// Like [`Self::new`], with the initial epoch fixed by the caller instead
    /// of drawn at random. A planned successor passes the `successor_epoch`
    /// its restart transaction fixed, so that the epoch in hello and in
    /// subscription cursors is the one its endpoint record publishes.
    ///
    /// `epoch` must be nonzero, and below `u64::MAX` because an invalidation
    /// advances it; a cold start passes `None`.
    pub fn with_epoch(
        runtime: AgentRuntime,
        handle: &tokio::runtime::Handle,
        limits: SubscriptionLimits,
        epoch: Option<u64>,
    ) -> Arc<Self> {
        Self::with_activity_clock(
            runtime,
            handle,
            limits,
            epoch,
            Arc::new(|| event_bus::EventMeta::now().monotonic),
        )
    }
    fn with_activity_clock(
        runtime: AgentRuntime,
        handle: &tokio::runtime::Handle,
        limits: SubscriptionLimits,
        epoch: Option<u64>,
        activity_now: Arc<dyn Fn() -> std::time::Duration + Send + Sync>,
    ) -> Arc<Self> {
        assert!(
            epoch.is_none_or(|e| e != 0 && e != u64::MAX),
            "a fixed projection epoch must be nonzero and advanceable"
        );
        let bus = runtime.event_bus();
        let mut events = bus.subscribe();
        let registry = ApprovalRegistry::for_bus(&bus);
        let mut approvals = registry.subscribe();
        let bodies_wake = Arc::new(tokio::sync::Notify::new());
        let wake = bodies_wake.clone();
        let observer: Arc<dyn Fn() + Send + Sync> = Arc::new(move || wake.notify_one());
        runtime.observe_event_commits(&observer);
        let epoch = epoch.unwrap_or_else(|| {
            u64::from_be_bytes(uuid::Uuid::new_v4().as_bytes()[..8].try_into().unwrap()) >> 1
        });
        let epoch_tx = Arc::new(tokio::sync::watch::Sender::new(epoch));
        let this = Arc::new(Self {
            activity_now,
            activity_redactor: secret_guard::SecretRedactor::from_env(),
            #[cfg(test)]
            activity_formats: std::sync::atomic::AtomicUsize::new(0),
            _commit_observer: observer,
            runtime,
            registry,
            epoch: epoch_tx.clone(),
            status: Mutex::new(Models::default().status()),
            pager: Pager::default(),
            bodies: Bodies::default(),
            body_cursor: Mutex::default(),
            bodies_wake: bodies_wake.clone(),
            cut_taken: AtomicBool::new(false),
            limits,
            model_refresh: Mutex::default(),
            #[cfg(test)]
            model_refreshes: tokio::sync::watch::Sender::new(0),
            #[cfg(test)]
            model_event_batches: tokio::sync::watch::Sender::new(0),
            state: Mutex::new(State {
                #[cfg(test)]
                snapshot_hook: None,
                #[cfg(test)]
                refresh_hook: None,
                epoch,
                epoch_tx,
                sequence: 0,
                next_subscription: 0,
                models_ready: false,
                projects: vec![],
                project_names: Default::default(),
                models: Models::default(),
                activities: BTreeMap::new(),
                shell_jobs: Default::default(),
                replay: VecDeque::new(),
                replay_bytes: 0,
                subscribers: vec![],
                snapshots: VecDeque::new(),
                answer_provenance: BTreeMap::new(),
            }),
            tasks: Mutex::new(Vec::new()),
        });
        let weak = Arc::downgrade(&this);
        let models = handle.spawn(async move {
            let mut flush = tokio::time::interval(shell_jobs::OUTPUT_INTERVAL);
            flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut ready_at = tokio::time::Instant::now();
            let mut pending = RefreshWakeups::default();
            let mut observed = Vec::new();
            let mut approvals_closed = false;
            loop {
                enum Change {
                    Event(Result<event_bus::Event, RecvError>),
                    Approval(Result<ApprovalSnapshot, tokio::sync::broadcast::error::RecvError>),
                    Refresh,
                    ShellFlush,
                }
                // Keep draining fragments/dirty notifications during cooldown.
                // Sleeping the receiver itself can overflow the bus and change
                // the epoch under an in-flight core restart. Only activity and
                // approval identities need buffering; bound both before reading
                // more, retaining the bus's existing lag/resync behavior.
                let change = tokio::select! {
                    biased;
                    () = tokio::time::sleep_until(ready_at), if pending.dirty => Change::Refresh,
                    change = async {
                        tokio::select! {
                            e = events.recv(), if !pending.closed && pending.activities.len() < MAX_REFRESH_WAKEUPS => Change::Event(e),
                            a = approvals.recv(), if !approvals_closed && observed.len() < MAX_REFRESH_WAKEUPS => Change::Approval(a),
                            _ = flush.tick() => Change::ShellFlush,
                        }
                    } => change,
                };
                // Never retain the projection/runtime across a scheduler wait.
                let Some(this) = weak.upgrade() else { break };
                match (this, change) {
                    (this, Change::Event(event)) => {
                        let wakeups = RefreshWakeups::drain(&mut events, event).await;
                        if wakeups.lagged {
                            pending.activities.clear();
                            this.invalidate();
                        }
                        pending.dirty |= wakeups.dirty;
                        pending.closed |= wakeups.closed;
                        pending.activities.extend(wakeups.activities);
                        #[cfg(test)]
                        this.model_event_batches.send_modify(|count| *count += 1);
                        // Output-only batches still bypass storage collection.
                        // Keep their order behind any already-buffered activity.
                        if !pending.dirty && !pending.activities.is_empty() {
                            let activities = std::mem::take(&mut pending.activities);
                            Self::in_background(this, move |p| {
                                for event in activities {
                                    p.observe_shell(event.clone(), tokio::time::Instant::now());
                                    p.observe_activity(event);
                                }
                            }).await;
                        }
                    }
                    (_this, Change::Approval(Ok(approval))) => {
                        pending.dirty = true;
                        observed.push(approval.id);
                    }
                    (this, Change::Approval(Err(tokio::sync::broadcast::error::RecvError::Lagged(_)))) => {
                        pending.activities.clear();
                        pending.dirty = true;
                        this.invalidate();
                    }
                    (_this, Change::Approval(Err(tokio::sync::broadcast::error::RecvError::Closed))) => {
                        approvals_closed = true;
                    }
                    (this, Change::ShellFlush) => this.flush_shell(tokio::time::Instant::now()),
                    (this, Change::Refresh) => {
                        pending.dirty = false;
                        let activities = std::mem::take(&mut pending.activities);
                        let approvals = std::mem::take(&mut observed);
                        Self::in_background(this, move |p| {
                            if p.refresh_models_observed(&approvals).is_err() {
                                p.invalidate();
                                return;
                            }
                            for event in activities {
                                p.observe_shell(event.clone(), tokio::time::Instant::now());
                                p.observe_activity(event);
                            }
                        }).await;
                        // Completion-based throttle, not debounce: even the
                        // last dirty schedules a trailing pass if input stops.
                        ready_at = tokio::time::Instant::now() + MIN_REFRESH_INTERVAL;
                        #[cfg(test)]
                        if let Some(this) = weak.upgrade() {
                            this.model_refreshes.send_modify(|count| *count += 1);
                        }
                    }
                }
                if (pending.closed || approvals_closed) && !pending.dirty { break; }
                tokio::task::yield_now().await;
            }
        });
        let weak = Arc::downgrade(&this);
        let bodies = handle.spawn(async move {
            loop {
                bodies_wake.notified().await;
                let Some(this) = weak.upgrade() else { break };
                Self::in_background(this, |p| {
                    let _ = p.advance_bodies();
                })
                .await;
            }
        });
        this.tasks.lock().unwrap().extend([models, bodies]);
        this
    }
    async fn in_background(this: Arc<Self>, work: impl FnOnce(&Self) + Send + 'static) {
        let _ = tokio::task::spawn_blocking(move || work(&this)).await;
    }
    pub(crate) fn configure(&self, projects: Vec<(ProjectId, PathBuf)>) {
        let _refresh = self.model_refresh.lock().unwrap();
        self.state.lock().unwrap().projects = projects;
    }
    /// Sets the project directory and the names shown for it, and reads the
    /// models again so that a listing reflects them at once.
    pub(crate) fn configure_projects(
        &self,
        projects: Vec<(ProjectId, PathBuf)>,
        names: std::collections::BTreeMap<String, String>,
    ) -> Result<(), WorkbenchError> {
        {
            let mut refresh = self.model_refresh.lock().unwrap();
            let mut state = self.state.lock().unwrap();
            state.projects = projects;
            state.project_names = names;
            drop(state);
            self.refresh_models_serialized(&mut refresh, &[])?;
        }
        self.models_changed();
        Ok(())
    }
    /// Epoch used by the handshake and subscription cursors.
    pub fn instance_epoch(&self) -> InstanceEpoch {
        InstanceEpoch(*self.epoch.borrow())
    }

    pub fn epoch_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.epoch.subscribe()
    }

    /// As of the last refresh; unlike `GetCoreStatus`, never waits for one.
    pub fn cached_status(&self) -> CoreStatus {
        self.status.lock().unwrap().clone()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn retained_bodies(&self) -> usize {
        self.bodies.retained()
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn history_route(&self) -> Option<crate::queries::HistoryRoute> {
        self.bodies.history_route()
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
    /// Reads the models and publishes every committed message before returning.
    pub fn refresh(&self) -> Result<(), WorkbenchError> {
        self.refresh_models_serialized(&mut self.model_refresh.lock().unwrap(), &[])?;
        self.advance_bodies()
    }
    pub(crate) fn refresh_models(&self) -> Result<(), WorkbenchError> {
        self.refresh_models_observed(&[])
    }
    pub(crate) fn approval_changed(
        &self,
        id: &sandbox::approval::ApprovalId,
    ) -> Result<(), WorkbenchError> {
        self.refresh_models_observed(std::slice::from_ref(id))
    }
    fn refresh_models_observed(
        &self,
        observed: &[sandbox::approval::ApprovalId],
    ) -> Result<(), WorkbenchError> {
        self.refresh_models_serialized(&mut self.model_refresh.lock().unwrap(), observed)?;
        self.models_changed();
        Ok(())
    }
    /// The first advance takes the cut that decides which messages are history, so
    /// it runs before the caller sees the models; later ones run in the background.
    fn models_changed(&self) {
        if self.cut_taken.load(Ordering::Acquire) {
            self.bodies_wake.notify_one();
        } else {
            let _ = self.advance_bodies();
        }
    }
    /// Never holds the state lock or the shared storage reader while it reads.
    fn advance_bodies(&self) -> Result<(), WorkbenchError> {
        let mut cursor = self.body_cursor.lock().unwrap();
        let runs: Vec<String> = {
            let state = self.state.lock().unwrap();
            if !state.models_ready {
                return Ok(());
            }
            state.models.runs.keys().cloned().collect()
        };
        let cursor = &mut *cursor;
        if cursor.reader.is_none() {
            cursor.reader = self.runtime.open_storage_reader().and_then(Result::ok);
        }
        #[cfg(test)]
        let mut hook = cursor.advance_hook.take();
        let scanner = &mut cursor.scanner;
        let mut messages = Vec::new();
        let mut read = |db: &storage::Database| {
            #[cfg(test)]
            if let Some(hook) = hook.take() {
                hook();
            }
            self.bodies
                .changes(db, scanner, &runs, |m| messages.push(m))
        };
        let result = match &cursor.reader {
            Some(db) => read(db),
            None => self
                .runtime
                .with_storage_reader(read)
                .map(Option::unwrap_or_default),
        }
        .map_err(queries::storage_error);
        self.cut_taken
            .store(cursor.scanner.initialized(), Ordering::Release);
        let mut state = self.state.lock().unwrap();
        if result.as_ref().is_ok_and(|rewritten| !*rewritten) {
            for message in messages {
                if state.models.threads.contains_key(message.thread.as_str()) {
                    state.publish(
                        UiEvent::Conversation(ConversationEvent::MessageCommitted { message }),
                        self.limits,
                    );
                }
            }
        } else {
            state.invalidate(self.limits);
        }
        result.map(drop)
    }
    fn refresh_models_serialized(
        &self,
        refresh: &mut ModelRefresh,
        observed: &[sandbox::approval::ApprovalId],
    ) -> Result<(), WorkbenchError> {
        let (projects, names) = {
            let state = self.state.lock().unwrap();
            (state.projects.clone(), state.project_names.clone())
        };
        if refresh.reader.is_none() {
            refresh.reader = self
                .runtime
                .open_storage_reader()
                .transpose()
                .map_err(queries::storage_error)?;
        }
        let mut next = queries::collect(&self.runtime, refresh.reader.as_ref(), &projects, &names)
            .map_err(queries::storage_error)?;
        #[cfg(test)]
        {
            let hook = self.state.lock().unwrap().refresh_hook.take();
            if let Some(hook) = hook {
                hook();
            }
        }
        let mut state = self.state.lock().unwrap();
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
            .chain(observed.iter().cloned())
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
        state.models_ready = true;
        *self.status.lock().unwrap() = state.models.status();
        if removed {
            state.invalidate(self.limits);
        }
        for event in changes {
            state.publish(event, self.limits);
        }
        Ok(())
    }
    /// An in-flight refresh will publish after the snapshot's high-water cut.
    /// Use the last complete models instead of waiting for its database work;
    /// bootstrap still needs an initial model set.
    fn refresh_for_snapshot(&self) -> Result<(), WorkbenchError> {
        let ready = self.state.lock().unwrap().models_ready;
        let refresh = if ready {
            match self.model_refresh.try_lock() {
                Ok(refresh) => Some(refresh),
                Err(std::sync::TryLockError::WouldBlock) => None,
                Err(std::sync::TryLockError::Poisoned(error)) => panic!("{error}"),
            }
        } else {
            Some(self.model_refresh.lock().unwrap())
        };
        if let Some(mut refresh) = refresh {
            self.refresh_models_serialized(&mut refresh, &[])?;
        }
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
        let read = {
            let mut refresh = self.model_refresh.lock().unwrap();
            self.refresh_models_serialized(&mut refresh, &[])?;
            let state = self.state.lock().unwrap();
            read(&state.models)
        };
        self.models_changed();
        read
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
        if let ExecutionQuery::ReadShellOutput(q) = q {
            return self
                .state
                .lock()
                .unwrap()
                .shell_jobs
                .read(q)
                .map(ExecutionReply::ShellOutput);
        }
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
                                && (!q.pending_only || v.is_pending())
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
        if !matches!(q, LifecycleQuery::GetCoreStatus) {
            return Err(queries::error(
                ErrorCode::PermissionDenied,
                "management query requires verified ingress",
            ));
        }
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
        let subscription = self.subscribe_after_refresh(request);
        self.models_changed();
        subscription
    }
    fn subscribe_after_refresh(&self, request: &Subscribe) -> Result<Subscription, WorkbenchError> {
        self.refresh_for_snapshot()?;
        let mut state = self.state.lock().unwrap();
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
            let mut items = snapshot_items(&state.models, &request.scope);
            items.extend(
                state
                    .shell_jobs
                    .views()
                    .filter(|job| {
                        matches_scope(
                            &state.models,
                            &request.scope,
                            &UiEvent::Execution(ExecutionEvent::ShellJobChanged {
                                job: (*job).clone(),
                            }),
                        )
                    })
                    .cloned()
                    .map(SnapshotItem::ShellJob),
            );
            let mut snapshot_bytes = serde_json::to_vec(&items).unwrap().len();
            if snapshot_bytes > MAX_SNAPSHOT_BYTES {
                return Err(queries::error(
                    ErrorCode::Busy,
                    "snapshot exceeds retention budget; use a narrower scope",
                ));
            }
            let now = (self.activity_now)();
            for item in state.activities.values().filter_map(|live| {
                let tool = live.at(now);
                matches_scope(
                    &state.models,
                    &request.scope,
                    &UiEvent::Execution(ExecutionEvent::ToolStarted { tool: tool.clone() }),
                )
                .then_some(SnapshotItem::ToolActivity(tool))
            }) {
                // Optional live display must not turn an otherwise valid snapshot into Busy.
                let bytes = serde_json::to_vec(&item).unwrap().len() + 1;
                if snapshot_bytes + bytes <= MAX_SNAPSHOT_BYTES {
                    snapshot_bytes += bytes;
                    items.push(item);
                }
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
        let page = self.read_snapshot_after_refresh(request);
        self.models_changed();
        page
    }
    fn read_snapshot_after_refresh(
        &self,
        request: &ReadSnapshot,
    ) -> Result<SnapshotPage, WorkbenchError> {
        self.refresh_for_snapshot()?;
        let mut state = self.state.lock().unwrap();
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
    /// The goodbye of a core that is stopping on purpose, for a subscription a
    /// connection still serves. The connection sends it *before* it closes (see
    /// `evorch-core::unix`): the notice made by dropping the projection comes too
    /// late, when the connection tasks are already gone.
    pub fn shutdown_notice(&self, id: &SubscriptionId) -> EventEnvelope {
        let mut state = self.state.lock().unwrap();
        let cursor = state.next_cursor();
        EventEnvelope {
            subscription_id: id.clone(),
            cursor,
            caused_by_request_id: None,
            event: UiEvent::Projection(ProjectionEvent::SubscriptionClosed {
                reason: CloseReason::CoreShuttingDown,
            }),
        }
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
        for task in self.tasks.get_mut().unwrap().drain(..) {
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

/// Every thread besides `thread_of` that shows the event's subject.
fn also_for(event: &UiEvent) -> &[ThreadId] {
    match event {
        UiEvent::Lanes(workbench_protocol::namespaces::LanesEvent::LaneChanged {
            also_for,
            ..
        }) => also_for,
        _ => &[],
    }
}

fn thread_of(event: &UiEvent) -> Option<&ThreadId> {
    match event {
        UiEvent::Workspace(WorkspaceEvent::ThreadChanged { thread }) => Some(&thread.id),
        UiEvent::Execution(ExecutionEvent::ShellJobChanged { job }) => Some(&job.thread),
        UiEvent::Execution(ExecutionEvent::ShellJobOutput { thread, .. }) => Some(thread),
        UiEvent::Execution(ExecutionEvent::ToolStarted { tool }) => Some(&tool.thread),
        UiEvent::Execution(ExecutionEvent::ToolFinished { thread, .. }) => Some(thread),
        UiEvent::Execution(ExecutionEvent::RunChanged { run }) => Some(&run.thread),
        UiEvent::Execution(
            ExecutionEvent::ThreadExecutionChanged { thread, .. }
            | ExecutionEvent::ConversationPlanChanged { thread, .. },
        ) => Some(thread),
        UiEvent::Teams(workbench_protocol::namespaces::teams::TeamsEvent::TeamChanged { team }) => {
            Some(&team.thread)
        }
        UiEvent::Lanes(workbench_protocol::namespaces::LanesEvent::LaneChanged {
            lane, ..
        }) => Some(lane.lead_thread_id.as_ref().unwrap_or(&lane.thread_id)),
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
    let threads: Vec<&ThreadId> = [thread_of(event), routed]
        .into_iter()
        .flatten()
        .chain(also_for(event))
        .chain(match event {
            UiEvent::Lanes(workbench_protocol::namespaces::LanesEvent::LaneChanged {
                lane,
                ..
            }) => Some(&lane.thread_id),
            _ => None,
        })
        .collect();
    match scope {
        SubscriptionScope::Management => false,
        SubscriptionScope::All => true,
        SubscriptionScope::Thread { thread } => threads.contains(&thread),
        SubscriptionScope::Project { project } => threads.into_iter().any(|t| {
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
                SubscriptionScope::Management => false,
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
                q.is_pending()
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
