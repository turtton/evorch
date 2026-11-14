//! Idle time is measured only while all three retention conditions are false.
use std::time::Duration;
use tokio::time::Instant;

pub struct IdleGrace {
    grace: Duration,
    since: Option<Instant>,
}
impl IdleGrace {
    pub fn new(grace: Duration) -> Self {
        Self { grace, since: None }
    }
    pub fn update(&mut self, web_enabled: bool, clients: usize, busy: bool) -> Option<Instant> {
        if web_enabled || clients != 0 || busy {
            self.since = None;
            return None;
        }
        self.since
            .get_or_insert_with(Instant::now)
            .checked_add(self.grace)
    }
}

/// Signal handles are installed before publishing readiness, in both production and workers.
pub struct OperatorSignals {
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
}
impl OperatorSignals {
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
        })
    }
    async fn received(&mut self) {
        tokio::select! { _ = self.terminate.recv() => (), _ = self.interrupt.recv() => () }
    }
}
/// Shared signal/idle/restart arbitration. Client loss never cancels a restart attempt.
pub struct LifecycleDriver {
    pub restart: crate::restart::RestartCoordinator,
    idle: IdleGrace,
}
impl LifecycleDriver {
    pub fn new(host: &workbench_host::bootstrap::BootstrappedHost) -> Self {
        Self {
            restart: crate::restart::RestartCoordinator::new(host),
            idle: IdleGrace::new(Duration::from_secs(host.config.core.idle_grace_secs)),
        }
    }
    pub async fn run(
        &mut self,
        host: &workbench_host::bootstrap::BootstrappedHost,
        backend: std::sync::Arc<crate::backend::HostBackend>,
        listener: tokio::net::UnixListener,
        web_enabled: bool,
        mut signals: OperatorSignals,
    ) -> std::io::Result<()> {
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let mut clients = tokio::task::JoinSet::new();
        let mut events = host.runtime.event_bus().subscribe();
        let background = host.host.lock().unwrap().service().background_work_notify();
        let area_work = host.area_work_notify();
        let mut restart = self.restart.subscribe();
        let outcome = loop {
            host.host.lock().unwrap().service_mut().poll();
            let deadline = self.idle.update(
                web_enabled,
                clients.len(),
                host.busy() || self.restart.busy(),
            );
            let expired = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = signals.received() => break Ok(()),
                _ = background.notified() => (),
                _ = area_work.notified() => (),
                _ = restart.changed() => (),
                _ = expired => { if !web_enabled && clients.is_empty() && !host.busy() && !self.restart.busy() { break Ok(()); } },
                event = events.recv() => { if let Ok(event) = event { host.host.lock().unwrap().service_mut().observe_lifecycle(&event); } },
                _ = clients.join_next(), if !clients.is_empty() => (),
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => { if clients.len() < 64 { clients.spawn(crate::unix::connection(stream, backend.clone(), shutdown.subscribe())); } },
                    Err(error) => break Err(error),
                },
            }
        };
        self.restart.transition_to_shutdown();
        drop(listener);
        backend.close();
        let _ = shutdown.send(true);
        while clients.join_next().await.is_some() {}
        outcome
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn retained_states_reset_idle_origin_and_only_the_full_grace_can_exit() {
        let grace = Duration::from_secs(60);
        let mut idle = IdleGrace::new(grace);
        use crate::restart::RestartPhase::*;
        for phase in [
            Requested,
            WaitingSafePoint,
            Sealing,
            Sealed,
            Cancelling,
            FailedClosed,
        ] {
            assert!(phase.retains_process());
            assert!(idle.update(false, 0, phase.retains_process()).is_none());
            tokio::time::advance(grace * 2).await;
            assert!(idle.update(false, 0, phase.retains_process()).is_none());
        }
        assert!(!Running.retains_process());
        let start = Instant::now();
        assert_eq!(idle.update(false, 0, false), Some(start + grace));
        tokio::time::advance(grace / 2).await;
        assert_eq!(idle.update(false, 0, false), Some(start + grace));
        assert!(idle.update(false, 0, true).is_none());
        let resumed = Instant::now();
        assert_eq!(idle.update(false, 0, false), Some(resumed + grace));
        tokio::time::advance(grace).await;
        assert_eq!(idle.update(false, 0, false), Some(Instant::now()));
    }
}
