//! Private renderer state: receipts/events carry ids; only local, authorized queries read outputs.
use super::renderer::data::Revocation;
use super::renderer::{OwnerError, Phase, RendererOwner, Tab};
use crate::{
    areas::{AreaHandles, AreaTasks, ProjectionSource},
    bootstrap::{
        BootstrapError,
        restart::{HostCommandPermit, HostRestartGate, HostRestartPhase},
    },
    connections::{ConnectionEpoch, ConnectionHooks},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;
use workbench_protocol::{
    envelope::{Actor, Provenance, Surface},
    errors::{ErrorCode, RetryAdvice, WorkbenchError},
    events::UiEvent,
    ids::ClientId,
    namespaces::{artifacts::ArtifactRef, browser::*},
};
mod bind;
mod operations;
#[cfg(test)]
mod tests;
const GRACE: Duration = Duration::from_secs(30);
const RESULT_TTL: Duration = Duration::from_secs(60);
const MAX_RESULT_BYTES: usize = 2 * 1024 * 1024;
const MAX_CAPTURE_BYTES: usize = 64 * 1024 * 1024;
const MAX_SESSIONS: usize = 32;

pub(super) fn id() -> RendererId {
    RendererId::new(uuid::Uuid::new_v4().simple().to_string()).unwrap()
}
fn error(code: ErrorCode, message: &'static str) -> WorkbenchError {
    WorkbenchError::new(code, message, RetryAdvice::Never)
}
pub(crate) fn unavailable() -> WorkbenchError {
    error(ErrorCode::Busy, "renderer is unavailable")
}
fn denied() -> WorkbenchError {
    error(
        ErrorCode::PermissionDenied,
        "renderer access is not permitted",
    )
}
fn busy() -> WorkbenchError {
    error(ErrorCode::Busy, "renderer private resource budget is full")
}
/// The tab's 4K stream has all the subscriptions it may have. A subscription
/// ending frees a place, so deciding again after a fresh read is right.
fn four_k_busy() -> WorkbenchError {
    WorkbenchError::new(
        ErrorCode::Busy,
        "a 4K frame stream has reached its subscription limit",
        RetryAdvice::AfterResync,
    )
}
/// Whether the tab's frames are a 4K stream at 30fps or more, as the budget counts it.
fn is_four_k_stream(viewport: RendererViewport, fps: u8) -> bool {
    fps >= 30
        && renderer_ipc::Viewport {
            width: viewport.width,
            height: viewport.height,
        }
        .frame_bytes()
            == renderer_ipc::MAX_FRAME_BYTES
}
fn local(p: &Provenance) -> Result<&ClientId, WorkbenchError> {
    p.validate().map_err(|_| denied())?;
    if p.actor != Actor::Human || !matches!(p.surface, Surface::Desktop | Surface::Cli) {
        return Err(error(
            ErrorCode::PermissionDenied,
            "renderer surface is not permitted",
        ));
    }
    p.client_id.as_ref().ok_or_else(denied)
}
struct Session {
    reference: RendererSessionRef,
    client: ClientId,
    surface: Surface,
    connection_epoch: Option<ConnectionEpoch>,
    token: RendererId,
    bound_generation: Option<u64>,
    detached: Option<Instant>,
}
#[derive(Clone)]
struct LiveTab {
    public: RendererTab,
    physical: Tab,
    client: Option<ClientId>,
    observers: HashMap<ClientId, (bool, bool)>,
}
struct Operation {
    public: RendererOperation,
    client: ClientId,
    surface: Surface,
    session: Option<String>,
    tab: Option<Tab>,
    artifact: Option<ArtifactRef>,
    expires: Instant,
    bytes: usize,
    cancel: tokio::sync::watch::Sender<bool>,
}
struct Capture {
    tab: Tab,
    bytes: Arc<Vec<u8>>,
    expires: Instant,
    /// The Capture operation that produced it; the binding names it again.
    operation: RendererId,
    client: ClientId,
    surface: Surface,
    connection_epoch: Option<ConnectionEpoch>,
    /// Set once a data connection is bound: the transfer is then the only
    /// reader and the Capture goes with it.
    bound: Option<Revocation>,
}
struct Subscription {
    client: ClientId,
    surface: Surface,
    connection_epoch: Option<ConnectionEpoch>,
    session: Option<String>,
    tab: Tab,
    expires: Instant,
    token: RendererId,
    /// Set once a data connection is bound. The token is spent then; dropping
    /// the subscription drops this and ends the lease.
    bound: Option<Revocation>,
}
fn alive_after(bound: &Option<Revocation>, expires: Instant, now: Instant) -> bool {
    match bound {
        Some(revocation) => !revocation.finished(),
        None => expires > now,
    }
}
/// A copy of when the preview control grace of a disconnected client ends.
///
/// It names no client, session or tab: the core only learns that the process is
/// retained until `until` and then lets it go. `None` is no grace at all, also
/// after a deadline has passed. Readers poll it; it holds no lock or reference
/// into the service.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RendererReconnectGrace {
    pub until: Option<Instant>,
}
#[derive(Default)]
struct State {
    sessions: HashMap<String, Session>,
    tabs: HashMap<String, LiveTab>,
    operations: HashMap<String, Operation>,
    captures: HashMap<String, Capture>,
    subscriptions: HashMap<String, Subscription>,
}
pub(crate) struct RendererService {
    epoch: u64,
    owner: RendererOwner,
    store: Arc<runtime::artifacts::ArtifactStore>,
    tasks: AreaTasks,
    gate: HostRestartGate,
    connections: Arc<ConnectionHooks>,
    source: ProjectionSource,
    state: Mutex<State>,
}
impl RendererService {
    pub(super) fn new(
        handles: &AreaHandles,
        owner: RendererOwner,
    ) -> Result<Arc<Self>, BootstrapError> {
        let service = Arc::new(Self {
            epoch: handles.projection.instance_epoch().get(),
            owner,
            store: handles.artifacts.clone(),
            tasks: handles.tasks.clone(),
            gate: handles.restart_gate.clone(),
            connections: handles.connections.clone(),
            source: handles
                .source("browser")
                .map_err(|e| BootstrapError::from(e.to_string()))?,
            state: Mutex::default(),
        });
        service.start()?;
        Ok(service)
    }
    fn start(self: &Arc<Self>) -> Result<(), BootstrapError> {
        let service = self;
        let weak = Arc::downgrade(service);
        service.connections.register(Arc::new(move |client, ended| {
            if let Some(service) = weak.upgrade() {
                let mut state = service.state.lock().unwrap();
                for session in state.sessions.values_mut() {
                    if &session.client == client
                        && session.connection_epoch.is_none_or(|epoch| epoch <= ended)
                    {
                        session.detached = Some(Instant::now() + GRACE);
                    }
                }
                state.subscriptions.retain(|_, lease| {
                    &lease.client != client
                        || lease.connection_epoch.is_some_and(|epoch| epoch > ended)
                });
                state.captures.retain(|_, capture| {
                    &capture.client != client
                        || capture.connection_epoch.is_some_and(|epoch| epoch > ended)
                });
            }
        }));
        let weak = Arc::downgrade(service);
        let mut status = service.owner.subscribe();
        let mut restart = service.gate.subscribe();
        service
            .tasks
            .spawn("renderer-private-state", move |mut stop| async move {
                let mut clock = tokio::time::interval(Duration::from_secs(1));
                loop {
                    tokio::select! {
                        biased;
                        _ = stop.stopped() => break,
                        changed = restart.changed() => { if changed.is_err() { break; } },
                        changed = status.changed() => { if changed.is_err() { break; } },
                        _ = clock.tick() => {},
                    }
                    let Some(service) = weak.upgrade() else {
                        break;
                    };
                    service.prune();
                }
                if let Some(service) = weak.upgrade() {
                    service.clear_previews(RendererFailure::Closed);
                }
                Ok(())
            })
            .map_err(|e| BootstrapError::from(e.to_string()))?;
        Ok(())
    }
    /// The latest end among the unexpired grace periods of detached sessions.
    pub(crate) fn reconnect_grace(&self) -> RendererReconnectGrace {
        let now = Instant::now();
        let until = self
            .state
            .lock()
            .unwrap()
            .sessions
            .values()
            .filter_map(|session| session.detached)
            .filter(|until| *until > now)
            .max();
        RendererReconnectGrace { until }
    }
    fn publish(&self, event: BrowserEvent) {
        let _ = self.source.publish(UiEvent::Browser(event));
    }
    fn session<'a>(
        &self,
        state: &'a State,
        reference: &RendererSessionRef,
        p: &Provenance,
    ) -> Result<&'a Session, WorkbenchError> {
        let client = local(p)?;
        let session = state
            .sessions
            .get(reference.id.as_str())
            .ok_or_else(denied)?;
        if reference.core_epoch != self.epoch
            || &session.client != client
            || session.surface != p.surface
            || session.detached.is_some()
        {
            return Err(denied());
        }
        Ok(session)
    }
    fn tab<'a>(
        &self,
        state: &'a State,
        reference: &RendererTabRef,
        p: &Provenance,
        mutation: bool,
    ) -> Result<&'a LiveTab, WorkbenchError> {
        let client = local(p)?;
        let tab = state.tabs.get(reference.id.as_str()).ok_or_else(denied)?;
        if reference.core_epoch != self.epoch || &tab.public.reference != reference {
            return Err(denied());
        }
        let owned = tab.client.as_ref() == Some(client)
            && self.session(state, &tab.public.session, p).is_ok();
        let observed = tab.observers.get(client).is_some_and(|rights| rights.0);
        if (mutation && !owned) || (!mutation && !owned && !observed) {
            return Err(denied());
        }
        Ok(tab)
    }
    fn remove_session(&self, state: &mut State, key: &str, reason: RendererFailure) {
        state.sessions.remove(key);
        state
            .subscriptions
            .retain(|_, lease| lease.session.as_deref() != Some(key));
        let closing = state
            .tabs
            .iter()
            .filter(|(_, tab)| tab.client.is_some() && tab.public.session.id.as_str() == key)
            .map(|(id, tab)| (id.clone(), tab.physical))
            .collect::<Vec<_>>();
        for (id, tab) in closing {
            state.tabs.remove(&id);
            self.owner.close_later(tab);
        }
        for operation in state
            .operations
            .values_mut()
            .filter(|op| op.session.as_deref() == Some(key))
        {
            operation.cancel.send_replace(true);
            operation.public.state = RendererOperationState::Cancelled { reason };
            operation.bytes = 0;
        }
        let live = state
            .tabs
            .values()
            .map(|tab| tab.physical)
            .collect::<Vec<_>>();
        state
            .captures
            .retain(|_, capture| live.contains(&capture.tab));
    }
    fn clear_previews(&self, reason: RendererFailure) {
        let mut state = self.state.lock().unwrap();
        let sessions = state.sessions.keys().cloned().collect::<Vec<_>>();
        for key in sessions {
            self.remove_session(&mut state, &key, reason);
        }
        for operation in state.operations.values_mut() {
            operation.cancel.send_replace(true);
            operation.public.state = RendererOperationState::Cancelled { reason };
            operation.bytes = 0;
        }
        for tab in state.tabs.values_mut() {
            tab.observers.clear();
        }
        state.captures.clear();
        state.subscriptions.clear();
    }
    fn prune(&self) {
        if self.gate.phase() != HostRestartPhase::Open {
            self.clear_previews(RendererFailure::Restart);
        }
        let status = self.owner.status();
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        let expired = state
            .sessions
            .iter()
            .filter(|(_, session)| {
                session.detached.is_some_and(|until| until <= now)
                    || session.bound_generation.is_some_and(|generation| {
                        generation != status.generation
                            || !matches!(status.phase, Phase::Ready | Phase::Idle)
                    })
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in expired {
            self.remove_session(&mut state, &key, RendererFailure::Closed);
        }
        state.tabs.retain(|_, tab| {
            tab.physical.generation == status.generation
                && matches!(status.phase, Phase::Ready | Phase::Idle)
        });
        state.operations.retain(|_, op| {
            let keep = op.expires > now;
            if !keep {
                op.cancel.send_replace(true);
            }
            keep
        });
        let live = state
            .tabs
            .values()
            .map(|tab| tab.physical)
            .collect::<Vec<_>>();
        state.captures.retain(|_, capture| {
            alive_after(&capture.bound, capture.expires, now) && live.contains(&capture.tab)
        });
        state.subscriptions.retain(|_, lease| {
            alive_after(&lease.bound, lease.expires, now) && live.contains(&lease.tab)
        });
        for operation in state.operations.values_mut() {
            if operation
                .tab
                .is_some_and(|tab| tab.generation != status.generation)
            {
                operation.cancel.send_replace(true);
                operation.public.state = RendererOperationState::Cancelled {
                    reason: RendererFailure::StaleGeneration,
                };
                operation.bytes = 0;
            }
        }
    }
    fn reserve(
        &self,
        state: &mut State,
        p: &Provenance,
        session: Option<String>,
        tab: Option<Tab>,
    ) -> Result<RendererId, WorkbenchError> {
        let client = local(p)?.clone();
        if state.operations.len() >= MAX_RENDERER_OPERATIONS
            || state.operations.values().map(|op| op.bytes).sum::<usize>() >= MAX_RESULT_BYTES
        {
            return Err(busy());
        }
        let operation_id = id();
        let (cancel, _) = tokio::sync::watch::channel(false);
        let artifact = tab.and_then(|physical| {
            state
                .tabs
                .values()
                .find(|live| live.physical == physical)
                .map(|live| live.public.artifact.clone())
        });
        state.operations.insert(
            operation_id.as_str().to_owned(),
            Operation {
                public: RendererOperation {
                    operation_id: operation_id.clone(),
                    state: RendererOperationState::Running,
                },
                client,
                surface: p.surface,
                session,
                tab,
                artifact,
                expires: Instant::now() + RESULT_TTL,
                bytes: 0,
                cancel,
            },
        );
        Ok(operation_id)
    }
    fn complete(
        &self,
        operation_id: &RendererId,
        outcome: Result<RendererOutput, RendererFailure>,
    ) {
        if self.gate.phase() != HostRestartPhase::Open {
            self.clear_previews(RendererFailure::Restart);
        }
        let mut state = self.state.lock().unwrap();
        let bytes = outcome
            .as_ref()
            .ok()
            .and_then(|output| serde_json::to_vec(output).ok())
            .map_or(0, |bytes| bytes.len());
        let cdp_large = matches!(&outcome, Ok(RendererOutput::Cdp { result }) if serde_json::to_vec(result).map_or(true, |bytes| bytes.len() > MAX_CDP_RESULT_BYTES));
        let too_large = cdp_large
            || bytes > MAX_CDP_RESULT_BYTES + 4096
            || bytes + state.operations.values().map(|op| op.bytes).sum::<usize>()
                > MAX_RESULT_BYTES;
        if let Some(operation) = state.operations.get_mut(operation_id.as_str())
            && matches!(operation.public.state, RendererOperationState::Running)
        {
            operation.public.state = match outcome {
                Ok(_) if too_large => RendererOperationState::Failed {
                    reason: RendererFailure::ResultTooLarge,
                },
                Ok(output) => RendererOperationState::Completed { output },
                Err(reason) => RendererOperationState::Failed { reason },
            };
            operation.bytes = if too_large { 0 } else { bytes };
            operation.expires = Instant::now() + RESULT_TTL;
        }
        drop(state);
        self.publish(BrowserEvent::OperationChanged {
            id: operation_id.clone(),
        });
    }
    pub(crate) fn query(
        &self,
        query: &BrowserQuery,
        p: &Provenance,
    ) -> Result<BrowserReply, WorkbenchError> {
        local(p)?;
        self.prune();
        let state = self.state.lock().unwrap();
        Ok(match query {
            BrowserQuery::GetRendererStatus {} => {
                let status = self.owner.status();
                BrowserReply::Status(RendererStatus {
                    core_epoch: self.epoch,
                    generation: status.generation,
                    phase: match status.phase {
                        Phase::Stopped => RendererPhase::Stopped,
                        Phase::Starting => RendererPhase::Starting,
                        Phase::Ready => RendererPhase::Ready,
                        Phase::Idle => RendererPhase::Idle,
                        Phase::Stopping => RendererPhase::Stopping,
                        Phase::Failed => RendererPhase::Failed,
                    },
                    tabs_used: status.tabs as u32,
                    tabs_limit: MAX_RENDERER_TABS as u32,
                })
            }
            BrowserQuery::ListRendererTabs { offset, limit } => {
                let mut tabs = state
                    .tabs
                    .values()
                    .filter(|tab| {
                        tab.client.as_ref() == p.client_id.as_ref()
                            && self.tab(&state, &tab.public.reference, p, true).is_ok()
                    })
                    .map(|tab| tab.public.clone())
                    .collect::<Vec<_>>();
                tabs.sort_by(|a, b| a.reference.id.as_str().cmp(b.reference.id.as_str()));
                let start = (*offset as usize).min(tabs.len());
                let end = start.saturating_add(*limit as usize).min(tabs.len());
                let more = end < tabs.len();
                BrowserReply::Tabs {
                    tabs: tabs.drain(start..end).collect(),
                    more,
                }
            }
            BrowserQuery::GetRendererTab { tab } => {
                BrowserReply::Tab(Box::new(self.tab(&state, tab, p, false)?.public.clone()))
            }
            BrowserQuery::GetRendererOperation { operation_id } => {
                let operation = state
                    .operations
                    .get(operation_id.as_str())
                    .ok_or_else(denied)?;
                let owner = if let Some(session) = &operation.session {
                    state
                        .sessions
                        .get(session)
                        .is_some_and(|session| self.session(&state, &session.reference, p).is_ok())
                } else {
                    Some(&operation.client) == p.client_id.as_ref()
                        && operation.surface == p.surface
                };
                if !owner
                    && !(Some(&operation.client) == p.client_id.as_ref()
                        && operation.surface == p.surface
                        && matches!(
                            operation.public.state,
                            RendererOperationState::Cancelled { .. }
                        ))
                {
                    return Err(denied());
                }
                BrowserReply::Operation(operation.public.clone())
            }
        })
    }
    pub(crate) fn operation_artifact(&self, operation_id: &RendererId) -> Option<ArtifactRef> {
        self.state
            .lock()
            .unwrap()
            .operations
            .get(operation_id.as_str())
            .and_then(|operation| operation.artifact.clone())
    }
    pub(crate) fn command_artifact(
        &self,
        command: &BrowserCommand,
        p: &Provenance,
    ) -> Result<Option<ArtifactRef>, WorkbenchError> {
        let client = local(p)?;
        self.prune();
        let state = self.state.lock().unwrap();
        let (reference, grant, frames) = match command {
            BrowserCommand::ResizeRendererTab { tab, .. }
            | BrowserCommand::SetRendererStreaming { tab, .. }
            | BrowserCommand::SetRendererFrameRate { tab, .. }
            | BrowserCommand::RendererCdp { tab, .. }
            | BrowserCommand::SettleRendererTab { tab }
            | BrowserCommand::TakeRendererNotices { tab }
            | BrowserCommand::CaptureRendererTab { tab } => (tab, false, false),
            BrowserCommand::GrantRendererObserver { tab, .. } => (tab, true, false),
            BrowserCommand::SubscribeRendererFrames { tab } => (tab, false, true),
            _ => return Ok(None),
        };
        let tab = state.tabs.get(reference.id.as_str()).ok_or_else(denied)?;
        if reference != &tab.public.reference {
            return Err(denied());
        }
        let owned = self.tab(&state, reference, p, true).is_ok();
        if !owned
            && !(grant && tab.client.is_none())
            && !(frames && tab.observers.get(client).is_some_and(|rights| rights.1))
        {
            return Err(denied());
        }
        Ok(Some(tab.public.artifact.clone()))
    }
    /// The tabs the runs display, as the host holds them after the last sync.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn run_tabs(&self) -> Vec<RendererTab> {
        self.state
            .lock()
            .unwrap()
            .tabs
            .values()
            .filter(|tab| tab.client.is_none())
            .map(|tab| tab.public.clone())
            .collect()
    }
    pub(crate) fn sync_runs(&self, runs: Vec<(super::run_pages::RunTab, ArtifactRef)>) {
        self.prune();
        let mut state = self.state.lock().unwrap();
        state.tabs.retain(|key, tab| {
            tab.client.is_some()
                || runs
                    .iter()
                    .any(|(run, _)| &run.incarnation == key && run.tab == tab.physical)
        });
        for (run, artifact) in runs {
            let Ok(opaque) = RendererId::new(&run.incarnation) else {
                continue;
            };
            state
                .tabs
                .entry(run.incarnation)
                .or_insert_with(|| LiveTab {
                    physical: run.tab,
                    client: None,
                    observers: HashMap::new(),
                    public: RendererTab {
                        reference: RendererTabRef {
                            core_epoch: self.epoch,
                            generation: run.tab.generation,
                            id: opaque.clone(),
                        },
                        session: RendererSessionRef {
                            core_epoch: self.epoch,
                            id: opaque,
                        },
                        owner: RendererOwnerKind::Run,
                        artifact,
                        viewport: RendererViewport {
                            width: 1024,
                            height: 768,
                        },
                        fps: 30,
                        streaming: false,
                    },
                });
        }
    }
}
fn failure(error: OwnerError) -> RendererFailure {
    match error {
        OwnerError::Busy | OwnerError::Remote(renderer_ipc::ErrorKind::Busy) => {
            RendererFailure::Busy
        }
        OwnerError::StaleGeneration
        | OwnerError::Remote(renderer_ipc::ErrorKind::StaleGeneration) => {
            RendererFailure::StaleGeneration
        }
        OwnerError::TimedOut | OwnerError::Remote(renderer_ipc::ErrorKind::Timeout) => {
            RendererFailure::TimedOut
        }
        OwnerError::Invalid
        | OwnerError::Remote(
            renderer_ipc::ErrorKind::Invalid | renderer_ipc::ErrorKind::Unsupported,
        ) => RendererFailure::InvalidRequest,
        OwnerError::Remote(renderer_ipc::ErrorKind::TooLarge) => RendererFailure::ResultTooLarge,
        OwnerError::Remote(renderer_ipc::ErrorKind::NotFound) => RendererFailure::Closed,
        _ => RendererFailure::Unavailable,
    }
}
