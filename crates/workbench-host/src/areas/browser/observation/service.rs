//! Native-only observation. Terminal runs and Stop requests refuse new live
//! subscriptions and revoke existing leases. Retained stopped images belong
//! only to the GUI. Secrets/URL/activity never enter receipts or events.
use super::*;
use crate::{
    areas::{
        AreaHandles,
        browser::renderer::data::{DataPlane, Lease, Revocation},
    },
    connections::{ConnectionEpoch, ConnectionHooks},
};
use std::{collections::HashMap, sync::Mutex};
use tokio::sync::{Notify, OwnedSemaphorePermit};
mod authorization;
mod lifecycle;
use authorization::{Authority, LeasePort};
use workbench_protocol::{
    envelope::{Actor, Provenance, Surface},
    errors::{ErrorCode, RetryAdvice, WorkbenchError},
    ids::ClientId,
    namespaces::browser::{BrowserCommand, BrowserQuery},
};
struct Subscription {
    scope: ObservationScope,
    client: ClientId,
    epoch: ConnectionEpoch,
    token: ObservationId,
    _slot: Arc<OwnedSemaphorePermit>,
    lease: Option<Lease>,
    revoke: Option<Revocation>,
    updates: watch::Receiver<Option<Arc<ObservedPage>>>,
    revision: u64,
    expires: tokio::time::Instant,
}
/// Host-authorized native observation. Notifications use the existing browser
/// projection: they are globally distributed and replayed, not client-targeted.
/// They contain ONLY opaque subscription ID, revision and coarse state. These
/// reveal no URL, title, activity, pixels, private token or diagnostic detail.
/// An ID is correlation, never a capability: subscription reads/unsubscribe
/// require its owning native client and live connection epoch; bind additionally
/// requires the private single-use token. Metadata by scope independently checks
/// stored membership and live execution; it is never resolved from a public ID.
/// Client-targeted delivery is a follow-up if needed, not a new shared contract.
/// Byte bounds are resource limits, NOT secrecy/redaction.
pub struct ObservationService {
    core_epoch: u64,
    port: Option<Arc<dyn AgentObservationPort>>,
    authority: Authority,
    wake: Notify,
    publish: Arc<dyn Fn(ObservationEvent) + Send + Sync>,
    plane: DataPlane,
    connections: Arc<ConnectionHooks>,
    subscriptions: Mutex<HashMap<ObservationId, Subscription>>,
}
fn denied() -> WorkbenchError {
    WorkbenchError::new(
        ErrorCode::PermissionDenied,
        "agent observation is not permitted",
        RetryAdvice::Never,
    )
}
fn id() -> ObservationId {
    ObservationId::new(uuid::Uuid::new_v4().simple().to_string()).unwrap()
}
impl ObservationService {
    /// Test-only fake path. Production always installs stored/live authority.
    #[cfg(any(test, feature = "test-support"))]
    pub fn new(
        port: Option<Arc<dyn AgentObservationPort>>,
        plane: DataPlane,
        connections: Arc<ConnectionHooks>,
        core_epoch: u64,
    ) -> Arc<Self> {
        Self::build(
            port,
            plane,
            connections,
            core_epoch,
            Authority::fake(),
            Arc::new(|_| {}),
        )
    }
    fn build(
        port: Option<Arc<dyn AgentObservationPort>>,
        plane: DataPlane,
        connections: Arc<ConnectionHooks>,
        core_epoch: u64,
        authority: Authority,
        publish: Arc<dyn Fn(ObservationEvent) + Send + Sync>,
    ) -> Arc<Self> {
        let service = Arc::new(Self {
            core_epoch,
            port,
            plane,
            connections: connections.clone(),
            subscriptions: Mutex::default(),
            authority,
            wake: Notify::new(),
            publish,
        });
        let weak = Arc::downgrade(&service);
        connections.register(Arc::new(move |client, epoch| {
            if let Some(s) = weak.upgrade() {
                s.disconnect(client, epoch);
            }
        }));
        service
    }
    pub(crate) fn from_handles(
        handles: &AreaHandles,
        plane: DataPlane,
    ) -> Result<Arc<Self>, crate::bootstrap::BootstrapError> {
        let source = handles.source("browser")?;
        let service = Self::build(
            handles.agent_observation.clone(),
            plane,
            handles.connections.clone(),
            handles.projection.instance_epoch().get(),
            Authority::from_handles(handles),
            Arc::new(move |event| {
                // Opaque subscription id/revision/state only; no private payload.
                if let Err(error) = source.publish(workbench_protocol::events::UiEvent::Browser(
                    workbench_protocol::namespaces::browser::BrowserEvent::AgentObservationChanged(
                        event,
                    ),
                )) {
                    tracing::warn!(?error.code, "observation invalidation was not published");
                }
            }),
        );
        service.start_lifecycle(
            &handles.tasks,
            handles.bus.subscribe(),
            handles.restart_gate.clone(),
        )?;
        Ok(service)
    }
    fn connected(&self, client: &ClientId, epoch: ConnectionEpoch) -> bool {
        self.connections.epoch_of(client) == Some(epoch)
    }
    fn local(&self, p: &Provenance) -> Result<(ClientId, ConnectionEpoch), WorkbenchError> {
        p.validate().map_err(|_| denied())?;
        if p.actor != Actor::Human || p.surface != Surface::Desktop {
            return Err(denied());
        }
        let client = p.client_id.clone().ok_or_else(denied)?;
        let epoch = self.connections.epoch_of(&client).ok_or_else(denied)?;
        Ok((client, epoch))
    }
    fn port(&self) -> Result<&Arc<dyn AgentObservationPort>, ObservationState> {
        self.port.as_ref().ok_or(ObservationState::Unavailable)
    }
    fn current(&self, scope: &ObservationScope) -> Result<(), ObservationState> {
        if scope.core_epoch != self.core_epoch {
            return Err(ObservationState::Revoked);
        }
        self.authority.check(scope)?;
        if !self.port()?.current(scope) {
            return Err(ObservationState::Revoked);
        }
        Ok(())
    }
    fn page(&self, scope: &ObservationScope) -> Result<Arc<ObservedPage>, ObservationState> {
        let port = self.port()?;
        if scope.core_epoch != self.core_epoch {
            return Err(ObservationState::Revoked);
        }
        self.authority.check(scope)?;
        let page = port.snapshot(scope).map_err(state)?;
        if page.snapshot.scope != *scope || !page.validate() {
            return Err(ObservationState::Revoked);
        }
        self.current(scope)?;
        if page.snapshot.state != ObservationState::Live {
            return Err(page.snapshot.state);
        }
        Ok(page)
    }
    pub fn query(
        &self,
        q: &BrowserQuery,
        p: &Provenance,
    ) -> Result<ObservationReply, WorkbenchError> {
        let (client, epoch) = self.local(p)?;
        self.prune();
        let reply = match q {
            BrowserQuery::ListAgentRuns { project, thread } => {
                if !self.authority.target(project, thread) {
                    return Err(denied());
                }
                match self
                    .port()
                    .and_then(|port| port.list(project, thread).map_err(state))
                {
                    Ok(scopes) => ObservationReply::Runs {
                        scopes: scopes
                            .into_iter()
                            .filter(|s| {
                                &s.project == project && &s.thread == thread && self.page(s).is_ok()
                            })
                            .take(MAX_RUNS)
                            .collect(),
                    },
                    Err(e) => ObservationReply::State(e),
                }
            }
            BrowserQuery::GetAgentObservation { scope } => match self.page(scope) {
                Ok(page) => ObservationReply::Snapshot(Box::new(page.snapshot.clone())),
                Err(e) => ObservationReply::State(e),
            },
            BrowserQuery::GetAgentSubscription { subscription } => {
                let subscriptions = self.subscriptions.lock().unwrap();
                let s = subscriptions.get(subscription).ok_or_else(denied)?;
                if s.client != client || s.epoch != epoch || self.current(&s.scope).is_err() {
                    return Err(denied());
                }
                ObservationReply::Subscription {
                    subscription: subscription.clone(),
                    scope: s.scope.clone(),
                    token: s.lease.as_ref().map(|_| s.token.clone()),
                    state: if s.lease.is_some() {
                        ObservationState::Starting
                    } else {
                        ObservationState::Live
                    },
                }
            }
            _ => return Err(denied()),
        };
        reply.validate().map_err(|_| denied())?;
        if !self.connected(&client, epoch) {
            return Err(denied());
        }
        match &reply {
            ObservationReply::Runs { scopes } => {
                if let BrowserQuery::ListAgentRuns { project, thread } = q
                    && !self.authority.target(project, thread)
                {
                    return Err(denied());
                }
                if scopes.iter().any(|s| self.current(s).is_err()) {
                    return Err(denied());
                }
            }
            ObservationReply::Snapshot(page) => {
                self.current(&page.scope).map_err(|_| denied())?;
            }
            ObservationReply::Subscription { scope, .. } => {
                self.current(scope).map_err(|_| denied())?;
            }
            _ => (),
        }
        Ok(reply)
    }
    pub fn command(
        &self,
        c: BrowserCommand,
        p: &Provenance,
    ) -> Result<ObservationResult, WorkbenchError> {
        let (client, epoch) = self.local(p)?;
        self.prune();
        match c {
            BrowserCommand::SubscribeAgentFrames { scope } => {
                if let Err(e) = self.page(&scope) {
                    return Ok(ObservationResult::State(e));
                }
                let port = self.port().map_err(|_| denied())?.clone();
                let mut feed = match port.subscribe(&scope) {
                    Ok(feed) => feed,
                    Err(e) => return Ok(ObservationResult::State(state(e))),
                };
                // External source work may race Stop/replacement/disconnect.
                if let Err(e) = self.page(&scope) {
                    return Ok(ObservationResult::State(e));
                }
                let initial = feed.updates.borrow_and_update().clone();
                let Some(initial) = initial.filter(|p| {
                    p.validate()
                        && p.snapshot.scope == scope
                        && p.snapshot.state == ObservationState::Live
                }) else {
                    return Ok(ObservationResult::State(ObservationState::Revoked));
                };
                if feed.updates.has_changed().is_err() {
                    return Ok(ObservationResult::State(ObservationState::Revoked));
                }
                let mut all = self.subscriptions.lock().unwrap();
                if !self.connected(&client, epoch) {
                    return Err(denied());
                }
                if let Err(e) = self.current(&scope) {
                    return Ok(ObservationResult::State(e));
                }
                let Some(slot) = self.plane.frame_slot() else {
                    return Ok(ObservationResult::State(ObservationState::Capacity));
                };
                let updates = feed.updates.clone();
                let guarded = Arc::new(LeasePort {
                    port,
                    authority: self.authority.clone(),
                    connections: self.connections.clone(),
                    client: client.clone(),
                    epoch,
                });
                let (lease, revoke) =
                    DataPlane::agent_lease(slot.clone(), guarded, scope.clone(), feed);
                let subscription = id();
                all.insert(
                    subscription.clone(),
                    Subscription {
                        scope,
                        client,
                        epoch,
                        token: id(),
                        _slot: slot,
                        lease: Some(lease),
                        revoke: Some(revoke),
                        updates,
                        // Snapshot and watch publication need not be atomic.
                        revision: initial.snapshot.revision,
                        expires: tokio::time::Instant::now()
                            + std::time::Duration::from_millis(u64::from(
                                workbench_protocol::namespaces::browser::data::BIND_WINDOW_MS,
                            )),
                    },
                );
                self.notify(
                    &subscription,
                    all.get(&subscription).unwrap(),
                    ObservationState::Starting,
                );
                self.wake.notify_one();
                Ok(ObservationResult::Subscribed {
                    subscription,
                    state: ObservationState::Starting,
                })
            }
            BrowserCommand::UnsubscribeAgentFrames { subscription } => {
                let mut all = self.subscriptions.lock().unwrap();
                if !all
                    .get(&subscription)
                    .is_some_and(|s| s.client == client && s.epoch == epoch)
                {
                    return Err(denied());
                }
                let mut s = all.remove(&subscription).unwrap();
                self.end(&subscription, &mut s, ObservationState::Stopped);
                self.wake.notify_one();
                Ok(ObservationResult::Unsubscribed {})
            }
            BrowserCommand::BindAgentData {
                subscription,
                token,
                channel_id,
            } => {
                let scope = {
                    let all = self.subscriptions.lock().unwrap();
                    let s = all.get(&subscription).ok_or_else(denied)?;
                    if s.client != client || s.epoch != epoch || s.token != token {
                        return Err(denied());
                    }
                    s.scope.clone()
                };
                // Do not hold the registry during external snapshot work.
                self.page(&scope).map_err(|_| denied())?;
                let mut all = self.subscriptions.lock().unwrap();
                let s = all.get_mut(&subscription).ok_or_else(denied)?;
                if s.client != client
                    || s.epoch != epoch
                    || s.token != token
                    || s.expires <= tokio::time::Instant::now()
                    || !self.connected(&client, epoch)
                    || self.current(&s.scope).is_err()
                    || !self.plane.is_pending(&channel_id)
                {
                    return Err(denied());
                }
                let lease = s.lease.take().ok_or_else(denied)?;
                if !self.plane.bind(&channel_id, lease) {
                    let mut s = all.remove(&subscription).unwrap();
                    self.end(&subscription, &mut s, ObservationState::Revoked);
                    return Err(denied());
                }
                self.notify(&subscription, s, ObservationState::Live);
                self.wake.notify_one();
                Ok(ObservationResult::Bound {})
            }
            _ => Err(denied()),
        }
    }
}
fn state(e: ObservationFailure) -> ObservationState {
    match e {
        ObservationFailure::Unavailable => ObservationState::Unavailable,
        ObservationFailure::Revoked => ObservationState::Revoked,
        ObservationFailure::Capacity => ObservationState::Capacity,
    }
}

#[cfg(test)]
mod tests;
