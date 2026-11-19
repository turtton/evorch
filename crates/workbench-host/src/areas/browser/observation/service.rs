//! O-H root. The integration path is deliberately small; production source is
//! unavailable until O-S. Subscription secrets are private query results only.
use super::*;
use crate::{
    areas::{
        AreaHandles,
        browser::renderer::data::{DataPlane, Lease, Revocation},
    },
    connections::{ConnectionEpoch, ConnectionHooks},
};
use std::{collections::HashMap, sync::Mutex};
use tokio::sync::OwnedSemaphorePermit;
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
    _revoke: Revocation,
    expires: tokio::time::Instant,
}
pub struct ObservationService {
    core_epoch: u64,
    port: Option<Arc<dyn AgentObservationPort>>,
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
    pub fn new(
        port: Option<Arc<dyn AgentObservationPort>>,
        plane: DataPlane,
        connections: Arc<ConnectionHooks>,
        core_epoch: u64,
    ) -> Arc<Self> {
        let service = Arc::new(Self {
            core_epoch,
            port,
            plane,
            connections: connections.clone(),
            subscriptions: Mutex::default(),
        });
        let weak = Arc::downgrade(&service);
        connections.register(Arc::new(move |client, epoch| {
            if let Some(s) = weak.upgrade() {
                s.subscriptions
                    .lock()
                    .unwrap()
                    .retain(|_, v| &v.client != client || v.epoch > epoch);
            }
        }));
        service
    }
    pub(crate) fn from_handles(
        handles: &AreaHandles,
        plane: DataPlane,
    ) -> Result<Arc<Self>, crate::bootstrap::BootstrapError> {
        let service = Self::new(
            handles.agent_observation.clone(),
            plane,
            handles.connections.clone(),
            handles.projection.instance_epoch().get(),
        );
        let weak = Arc::downgrade(&service);
        let gate = handles.restart_gate.clone();
        let mut restart = gate.subscribe();
        handles
            .tasks
            .spawn("agent-observation-lifecycle", move |mut stop| async move {
                let mut clock = tokio::time::interval(std::time::Duration::from_secs(1));
                loop {
                    tokio::select! {
                        biased;
                        _=stop.stopped()=>break,
                        changed=restart.changed()=>{if changed.is_err(){break;}},
                        _=clock.tick()=>(),
                    }
                    let Some(service) = weak.upgrade() else {
                        return Ok(());
                    };
                    if gate.phase() != crate::bootstrap::restart::HostRestartPhase::Open {
                        service.subscriptions.lock().unwrap().clear();
                    } else {
                        service.prune();
                    }
                }
                if let Some(service) = weak.upgrade() {
                    service.subscriptions.lock().unwrap().clear();
                }
                Ok(())
            })
            .map_err(|e| crate::bootstrap::BootstrapError::from(e.to_string()))?;
        Ok(service)
    }
    fn prune(&self) {
        let now = tokio::time::Instant::now();
        self.subscriptions.lock().unwrap().retain(|_, s| {
            self.port.as_ref().is_some_and(|p| p.current(&s.scope))
                && !(s.lease.is_some() && s.expires <= now)
                && !s._revoke.finished()
        });
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
    fn port(&self) -> Result<&Arc<dyn AgentObservationPort>, ObservationFailure> {
        self.port.as_ref().ok_or(ObservationFailure::Unavailable)
    }
    fn page(&self, scope: &ObservationScope) -> Result<Arc<ObservedPage>, ObservationFailure> {
        if scope.core_epoch != self.core_epoch {
            return Err(ObservationFailure::Revoked);
        }
        let port = self.port()?;
        let page = port.snapshot(scope)?;
        if page.snapshot.scope != *scope || !page.validate() || !port.current(scope) {
            return Err(ObservationFailure::Revoked);
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
                match self.port().and_then(|port| port.list(project, thread)) {
                    Ok(scopes) => ObservationReply::Runs {
                        scopes: scopes
                            .into_iter()
                            .filter(|s| {
                                &s.project == project && &s.thread == thread && self.page(s).is_ok()
                            })
                            .take(MAX_RUNS)
                            .collect(),
                    },
                    Err(e) => ObservationReply::State(state(e)),
                }
            }
            BrowserQuery::GetAgentObservation { scope } => match self.page(scope) {
                Ok(page) => ObservationReply::Snapshot(Box::new(page.snapshot.clone())),
                Err(e) => ObservationReply::State(state(e)),
            },
            BrowserQuery::GetAgentSubscription { subscription } => {
                let subscriptions = self.subscriptions.lock().unwrap();
                let s = subscriptions.get(subscription).ok_or_else(denied)?;
                if s.client != client || s.epoch != epoch || self.page(&s.scope).is_err() {
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
                    return Ok(ObservationResult::State(state(e)));
                }
                let port = self.port().map_err(|_| denied())?.clone();
                let feed = port.subscribe(&scope).map_err(|_| denied())?;
                let Some(slot) = self.plane.frame_slot() else {
                    return Ok(ObservationResult::State(ObservationState::Capacity));
                };
                let (lease, revoke) =
                    DataPlane::agent_lease(slot.clone(), port, scope.clone(), feed);
                let subscription = id();
                let token = id();
                self.subscriptions.lock().unwrap().insert(
                    subscription.clone(),
                    Subscription {
                        scope,
                        client,
                        epoch,
                        token,
                        _slot: slot,
                        lease: Some(lease),
                        _revoke: revoke,
                        expires: tokio::time::Instant::now()
                            + std::time::Duration::from_millis(u64::from(
                                workbench_protocol::namespaces::browser::data::BIND_WINDOW_MS,
                            )),
                    },
                );
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
                all.remove(&subscription);
                Ok(ObservationResult::Unsubscribed {})
            }
            BrowserCommand::BindAgentData {
                subscription,
                token,
                channel_id,
            } => {
                let mut all = self.subscriptions.lock().unwrap();
                let s = all.get_mut(&subscription).ok_or_else(denied)?;
                if s.client != client
                    || s.epoch != epoch
                    || s.token != token
                    || self.page(&s.scope).is_err()
                    || !self.plane.is_pending(&channel_id)
                {
                    return Err(denied());
                }
                let lease = s.lease.take().ok_or_else(denied)?;
                if !self.plane.bind(&channel_id, lease) {
                    return Err(denied());
                }
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
mod tests {
    use super::*;
    use crate::areas::browser::observation::{ObservedFrame, fake::FakeObservationPort};
    use workbench_protocol::ids::{ProjectId, RunId, ThreadId};
    fn scope() -> ObservationScope {
        ObservationScope {
            project: ProjectId::new("p").unwrap(),
            thread: ThreadId::new("t").unwrap(),
            run: RunId::new("run").unwrap(),
            incarnation: RunId::new("incarnation").unwrap(),
            core_epoch: 7,
            browser_generation: 1,
            page_generation: 1,
        }
    }
    fn page() -> ObservedPage {
        let scope = scope();
        let tab = id();
        ObservedPage {
            snapshot: ObservationSnapshot {
                scope: scope.clone(),
                revision: 1,
                state: ObservationState::Live,
                tabs: vec![ObservationTab {
                    id: tab.clone(),
                    title: "title".into(),
                    url: "https://example.test/private".into(),
                }],
                active_tab: Some(tab.clone()),
                activity: vec![],
            },
            frame: Some(Arc::new(ObservedFrame {
                start: data::AgentFrameStart {
                    scope,
                    tab,
                    sequence: 1,
                    width: 1,
                    height: 1,
                    stride: 4,
                    total_bytes: 4,
                },
                rgba: Arc::from([1, 2, 3, 255]),
            })),
        }
    }
    #[tokio::test(start_paused = true)]
    async fn unused_subscription_expires_at_the_common_bind_window() {
        let port = Arc::new(FakeObservationPort::default());
        assert!(port.publish(page()));
        let plane = DataPlane::default();
        let connections = Arc::new(ConnectionHooks::default());
        let client = ClientId::new("desktop").unwrap();
        let _connection = connections.open(client.clone());
        let p = Provenance::human(Surface::Desktop, client);
        let service = ObservationService::new(Some(port), plane.clone(), connections, 7);
        let ObservationResult::Subscribed { subscription, .. } = service
            .command(BrowserCommand::SubscribeAgentFrames { scope: scope() }, &p)
            .unwrap()
        else {
            panic!()
        };
        tokio::time::advance(std::time::Duration::from_millis(u64::from(
            workbench_protocol::namespaces::browser::data::BIND_WINDOW_MS,
        )))
        .await;
        assert!(
            service
                .query(&BrowserQuery::GetAgentSubscription { subscription }, &p)
                .is_err()
        );
        let slots = (0..8)
            .map(|_| plane.frame_slot().unwrap())
            .collect::<Vec<_>>();
        assert!(plane.frame_slot().is_none());
        drop(slots);
    }
    #[tokio::test]
    async fn fake_query_subscribe_bind_receive_cancel_and_closing_slot() {
        let port = Arc::new(FakeObservationPort::default());
        assert!(port.publish(page()));
        let plane = DataPlane::default();
        let connections = Arc::new(ConnectionHooks::default());
        let client = ClientId::new("desktop").unwrap();
        let _connection = connections.open(client.clone());
        let p = Provenance::human(Surface::Desktop, client);
        let service = ObservationService::new(Some(port.clone()), plane.clone(), connections, 7);
        assert!(
            matches!(service.query(&BrowserQuery::ListAgentRuns{project:scope().project,thread:scope().thread},&p).unwrap(),ObservationReply::Runs{scopes} if scopes==vec![scope()])
        );
        let ObservationResult::Subscribed { subscription, .. } = service
            .command(BrowserCommand::SubscribeAgentFrames { scope: scope() }, &p)
            .unwrap()
        else {
            panic!()
        };
        let ObservationReply::Subscription {
            token: Some(token),
            state: ObservationState::Starting,
            ..
        } = service
            .query(
                &BrowserQuery::GetAgentSubscription {
                    subscription: subscription.clone(),
                },
                &p,
            )
            .unwrap()
        else {
            panic!()
        };
        let channel = plane.open_channel().unwrap();
        let channel_id = channel.id().clone();
        let bind = BrowserCommand::BindAgentData {
            subscription: subscription.clone(),
            token,
            channel_id,
        };
        service.command(bind.clone(), &p).unwrap();
        assert!(service.command(bind, &p).is_err());
        let Lease::AgentFrames(mut lease) = channel.bound().await.unwrap() else {
            panic!()
        };
        assert_eq!(&*lease.next().await.unwrap().rgba, &[1, 2, 3, 255]);
        // Renderer and agent observation draw from the SAME eight slots.
        let renderer_slots = (0..7)
            .map(|_| plane.frame_slot().unwrap())
            .collect::<Vec<_>>();
        assert!(plane.frame_slot().is_none());
        service
            .command(BrowserCommand::UnsubscribeAgentFrames { subscription }, &p)
            .unwrap();
        assert!(!lease.current());
        assert!(lease.next().await.is_none());
        assert!(plane.frame_slot().is_none(), "closing lease still counts");
        drop(lease);
        assert!(plane.frame_slot().is_some());
        drop(renderer_slots);
    }
    #[tokio::test]
    async fn revoked_scope_wrong_client_surface_and_epoch_are_refused() {
        let port = Arc::new(FakeObservationPort::default());
        assert!(port.publish(page()));
        let connections = Arc::new(ConnectionHooks::default());
        let client = ClientId::new("desktop").unwrap();
        let connection = connections.open(client.clone());
        let p = Provenance::human(Surface::Desktop, client.clone());
        let service = ObservationService::new(
            Some(port.clone()),
            DataPlane::default(),
            connections.clone(),
            7,
        );
        let ObservationResult::Subscribed { subscription, .. } = service
            .command(BrowserCommand::SubscribeAgentFrames { scope: scope() }, &p)
            .unwrap()
        else {
            panic!()
        };
        let query = BrowserQuery::GetAgentSubscription { subscription };
        for surface in [Surface::Web, Surface::Cli, Surface::None] {
            assert!(
                service
                    .query(&query, &Provenance::human(surface, client.clone()))
                    .is_err()
            );
        }
        let other = ClientId::new("other").unwrap();
        let _other = connections.open(other.clone());
        assert!(
            service
                .query(&query, &Provenance::human(Surface::Desktop, other))
                .is_err()
        );
        let mut old = scope();
        old.page_generation += 1;
        assert!(matches!(
            service
                .query(&BrowserQuery::GetAgentObservation { scope: old }, &p)
                .unwrap(),
            ObservationReply::State(ObservationState::Revoked)
        ));
        port.revoke(&scope());
        assert!(service.query(&query, &p).is_err());
        drop(connection);
        let _new = connections.open(client);
        assert!(service.query(&query, &p).is_err());
    }
    #[test]
    fn coherent_publication_rejects_new_tab_with_old_pixels_and_absent_backend_is_closed() {
        let mut page = page();
        page.snapshot.active_tab = Some(id());
        assert!(!page.validate());
        let port = FakeObservationPort::default();
        assert!(!port.publish(page));
        let connections = Arc::new(ConnectionHooks::default());
        let client = ClientId::new("desktop").unwrap();
        let _connection = connections.open(client.clone());
        let service = ObservationService::new(None, DataPlane::default(), connections, 7);
        assert!(matches!(
            service
                .query(
                    &BrowserQuery::GetAgentObservation { scope: scope() },
                    &Provenance::human(Surface::Desktop, client)
                )
                .unwrap(),
            ObservationReply::State(ObservationState::Unavailable)
        ));
    }
}
