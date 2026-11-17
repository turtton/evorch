//! Observe connection and detached-work generations without acknowledging output.
use super::IdleGrace;
use crate::backend::HostBackend;
use workbench_web::{ActivityObserver, WebServer};

struct Observed {
    activity: ActivityObserver,
    generation: u64,
}

impl Observed {
    fn new(activity: ActivityObserver) -> Self {
        let generation = activity.snapshot().2;
        Self {
            activity,
            generation,
        }
    }

    fn has_changed(&self) -> bool {
        self.activity.snapshot().2 != self.generation
    }

    fn observe(&mut self, idle: &mut IdleGrace) -> (usize, bool) {
        let (count, unconfirmed, generation) = self.activity.snapshot();
        if generation != self.generation {
            idle.reset();
            self.generation = generation;
        }
        (count, unconfirmed)
    }

    async fn changed(&self) {
        self.activity.changed(self.generation).await;
    }
}

pub(super) struct WebActivity {
    clients: Option<Observed>,
    accepted: Option<Observed>,
    deliveries: Observed,
}

impl WebActivity {
    pub(super) fn new(web: Option<&WebServer>, backend: &HostBackend) -> Self {
        let clients = web.map(WebServer::connected_clients);
        if let Some(clients) = &clients {
            backend.track_web_clients(clients.clone());
        }
        Self {
            clients: clients.map(Observed::new),
            accepted: web.map(|web| Observed::new(web.accepted_activity())),
            deliveries: Observed::new(backend.web_delivery_activity()),
        }
    }

    pub(super) fn has_changed(&self) -> bool {
        self.clients.as_ref().is_some_and(Observed::has_changed)
            || self.accepted.as_ref().is_some_and(Observed::has_changed)
            || self.deliveries.has_changed()
    }

    pub(super) fn observe(&mut self, idle: &mut IdleGrace) -> (usize, bool) {
        // Generations preserve a full connect/disconnect or accepted-task
        // start/end pair between driver polls. All returned values are copies.
        let clients = self
            .clients
            .as_mut()
            .map_or((0, false), |a| a.observe(idle));
        let accepted = self
            .accepted
            .as_mut()
            .map_or((0, false), |a| a.observe(idle));
        let deliveries = self.deliveries.observe(idle);
        (
            clients.0,
            clients.1 || accepted.0 != 0 || accepted.1 || deliveries.0 != 0 || deliveries.1,
        )
    }

    pub(super) async fn changed(&self) {
        tokio::select! {
            _ = optional_change(&self.clients) => (),
            _ = optional_change(&self.accepted) => (),
            _ = self.deliveries.changed() => (),
        }
    }
}

async fn optional_change(activity: &Option<Observed>) {
    match activity {
        Some(activity) => activity.changed().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::Instant;
    use workbench_web::RestartTasks;

    #[tokio::test(start_paused = true)]
    async fn detached_activity_resets_grace_even_when_it_finishes_between_polls() {
        let accepted = RestartTasks::default();
        let deliveries = RestartTasks::default();
        let mut activity = WebActivity {
            clients: None,
            accepted: Some(Observed::new(accepted.activity())),
            deliveries: Observed::new(deliveries.activity()),
        };
        let grace = Duration::from_secs(60);
        let mut idle = IdleGrace::new(grace);
        assert_eq!(activity.observe(&mut idle), (0, false));
        let first = idle.update(false, 0, false).unwrap();
        tokio::time::advance(grace / 2).await;
        accepted.spawn(async {}).unwrap().await.unwrap();
        assert!(activity.has_changed());
        assert_eq!(activity.observe(&mut idle), (0, false));
        let after = idle.update(false, 0, false).unwrap();
        assert_eq!(after, Instant::now() + grace);
        assert_ne!(after, first);

        let task = accepted.spawn(std::future::pending::<()>()).unwrap();
        let (clients, busy) = activity.observe(&mut idle);
        assert!(idle.update(false, clients, busy).is_none());
        task.abort();
        assert!(task.await.is_err());
        assert_eq!(activity.observe(&mut idle), (0, true));
    }
}
