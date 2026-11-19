//! One tracked watcher for all (at most eight) leases. Watch versions and a
//! retained Notify permit close the query/subscribe/wait registration races.
use super::*;
use crate::{
    areas::AreaTasks,
    bootstrap::{
        BootstrapError,
        restart::{HostRestartGate, HostRestartPhase},
    },
};
use std::{future::Future, task::Poll};
impl ObservationService {
    // Registry mutation and publication share this lock, so an old Live change
    // cannot overtake a terminal notification from another request.
    pub(super) fn notify(&self, id: &ObservationId, s: &Subscription, state: ObservationState) {
        (self.publish)(ObservationEvent {
            subscription: id.clone(),
            revision: s.revision,
            state,
        });
    }
    // Close the data authority BEFORE publishing a terminal notification.
    pub(super) fn end(&self, id: &ObservationId, s: &mut Subscription, state: ObservationState) {
        drop(s.revoke.take());
        self.notify(id, s, state);
    }
    pub(super) fn disconnect(&self, client: &ClientId, epoch: ConnectionEpoch) {
        self.subscriptions.lock().unwrap().retain(|id, s| {
            if &s.client != client || s.epoch > epoch {
                return true;
            }
            self.end(id, s, ObservationState::Revoked);
            false
        });
        self.wake.notify_one();
    }
    fn clear(&self) {
        self.subscriptions.lock().unwrap().retain(|id, s| {
            self.end(id, s, ObservationState::Revoked);
            false
        });
    }
    pub(super) fn prune(&self) {
        let now = tokio::time::Instant::now();
        self.subscriptions.lock().unwrap().retain(|id, s| {
            let page = s.updates.borrow_and_update().clone();
            let end = self.current(&s.scope).err().or_else(|| {
                if !self.connected(&s.client, s.epoch)
                    || (s.lease.is_some() && s.expires <= now)
                    || s.revoke.as_ref().is_none_or(Revocation::finished)
                    || s.updates.has_changed().is_err()
                {
                    Some(ObservationState::Revoked)
                } else {
                    match &page {
                        Some(p) if p.validate() && p.snapshot.scope == s.scope => {
                            (p.snapshot.state != ObservationState::Live).then_some(p.snapshot.state)
                        }
                        _ => Some(ObservationState::Revoked),
                    }
                }
            });
            if let Some(state) = end {
                self.end(id, s, state);
                return false;
            }
            let page = page.unwrap();
            if page.snapshot.revision != s.revision {
                // Monotone metadata revisions are part of a source generation.
                if page.snapshot.revision < s.revision {
                    self.end(id, s, ObservationState::Revoked);
                    return false;
                }
                s.revision = page.snapshot.revision;
                self.notify(
                    id,
                    s,
                    if s.lease.is_some() {
                        ObservationState::Starting
                    } else {
                        ObservationState::Live
                    },
                );
            }
            true
        });
    }
    /// Cloning keeps each receiver's unseen version. An update occurring before
    /// its changed future is polled is therefore still observed.
    async fn updated(&self) {
        let mut waits = self
            .subscriptions
            .lock()
            .unwrap()
            .values()
            .map(|s| {
                let mut rx = s.updates.clone();
                Box::pin(async move {
                    let _ = rx.changed().await;
                })
            })
            .collect::<Vec<_>>();
        std::future::poll_fn(|cx| {
            for wait in &mut waits {
                if wait.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(());
                }
            }
            Poll::Pending
        })
        .await;
    }
    pub(super) fn start_lifecycle(
        self: &Arc<Self>,
        tasks: &AreaTasks,
        mut events: event_bus::EventReceiver,
        gate: HostRestartGate,
    ) -> Result<(), BootstrapError> {
        let weak = Arc::downgrade(self);
        let mut restart = gate.subscribe();
        tasks.spawn("agent-observation-lifecycle", move |mut stop| async move {
            let mut clock = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                let Some(service) = weak.upgrade() else { return Ok(()); };
                if gate.phase() != HostRestartPhase::Open { service.clear(); } else { service.prune(); }
                tokio::select! {
                    biased;
                    _ = stop.stopped() => { service.clear(); break; }
                    change = restart.changed() => { if change.is_err() { service.clear(); break; } }
                    event = events.recv() => { if matches!(event, Err(event_bus::RecvError::Closed)) { service.clear(); break; } }
                    _ = service.wake.notified() => (),
                    _ = service.updated() => (),
                    // Persisted deletion can arrive from another process. Time
                    // is only a cleanup fallback, never an authorization grant.
                    _ = clock.tick() => (),
                }
            }
            Ok(())
        }).map_err(|e| BootstrapError::from(e.to_string()))
    }
}
