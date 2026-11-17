//! Idle grace applies only with no admitted clients and no retained work.
use std::time::Duration;
use tokio::time::Instant;
#[path = "lifecycle/idle/activity.rs"]
mod idle_activity;

pub struct IdleGrace {
    grace: Duration,
    since: Option<Instant>,
}
impl IdleGrace {
    pub fn new(grace: Duration) -> Self {
        Self { grace, since: None }
    }
    pub fn update(&mut self, web_enabled: bool, clients: usize, busy: bool) -> Option<Instant> {
        self.update_at(Instant::now(), web_enabled, clients, busy)
    }
    /// The caller supplies time so policy tests need no wall clock or timer.
    pub fn update_at(
        &mut self,
        now: Instant,
        untracked_web: bool,
        clients: usize,
        busy: bool,
    ) -> Option<Instant> {
        if untracked_web || clients != 0 || busy {
            self.reset();
            return None;
        }
        self.since.get_or_insert(now).checked_add(self.grace)
    }
    fn reset(&mut self) {
        self.since = None;
    }
}

#[cfg(target_os = "linux")]
pub mod signals;
#[cfg(target_os = "linux")]
pub use signals::OperatorSignals;
/// Signal handles are installed before publishing readiness, in both production and workers.
#[cfg(not(target_os = "linux"))]
pub struct OperatorSignals {
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
}
#[cfg(not(target_os = "linux"))]
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
pub mod control;
pub struct LifecycleDriver {
    control: control::ControlDrainHandle,
    control_requests: tokio::sync::mpsc::UnboundedReceiver<control::Request>,
    pub restart: crate::restart::RestartCoordinator,
    idle: IdleGrace,
}
impl LifecycleDriver {
    pub fn new(host: &workbench_host::bootstrap::BootstrappedHost) -> Self {
        let (send, control_requests) = tokio::sync::mpsc::unbounded_channel();
        Self {
            control: control::ControlDrainHandle(send),
            control_requests,
            restart: crate::restart::RestartCoordinator::new(host),
            idle: IdleGrace::new(Duration::from_secs(host.config.core.idle_grace_secs)),
        }
    }
    pub fn control_drain(&self) -> control::ControlDrainHandle {
        self.control.clone()
    }
    pub async fn run(
        &mut self,
        host: &workbench_host::bootstrap::BootstrappedHost,
        backend: std::sync::Arc<crate::backend::HostBackend>,
        listener: tokio::net::UnixListener,
        web_enabled: bool,
        signals: OperatorSignals,
    ) -> std::io::Result<()> {
        self.run_inner(host, backend, listener, web_enabled, None, signals)
            .await
    }
    /// Production transfers the actual web stop/join owner, not only an enabled flag.
    pub async fn run_with_web(
        &mut self,
        host: &workbench_host::bootstrap::BootstrappedHost,
        backend: std::sync::Arc<crate::backend::HostBackend>,
        listener: tokio::net::UnixListener,
        web: Option<workbench_web::WebServer>,
        signals: OperatorSignals,
    ) -> std::io::Result<()> {
        self.run_inner(host, backend, listener, web.is_some(), web, signals)
            .await
    }
    async fn run_inner(
        &mut self,
        host: &workbench_host::bootstrap::BootstrappedHost,
        backend: std::sync::Arc<crate::backend::HostBackend>,
        listener: tokio::net::UnixListener,
        web_enabled: bool,
        mut web: Option<workbench_web::WebServer>,
        mut signals: OperatorSignals,
    ) -> std::io::Result<()> {
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let mut web_activity = idle_activity::WebActivity::new(web.as_ref(), &backend);
        let mut listener = Some(listener);
        let mut clients = tokio::task::JoinSet::new();
        let mut client_failure = None;
        let mut events = host.runtime.event_bus().subscribe();
        let background = host.host.lock().unwrap().service().background_work_notify();
        let area_work = host.area_work_notify();
        let mut restart = self.restart.subscribe();
        let outcome = loop {
            host.host.lock().unwrap().service_mut().poll();
            let (web_count, web_busy) = web_activity.observe(&mut self.idle);
            let deadline = self.idle.update(
                // The legacy entry point cannot prove an externally owned web
                // listener empty. Production always transfers the actual owner.
                web_enabled && web.is_none(),
                clients.len() + web_count,
                host.busy() || self.restart.busy() || web_busy,
            );
            let expired = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = signals.received() => break Ok(()),
                Some(request) = self.control_requests.recv() => {
                    // A caller that retained web ownership cannot supply a receipt.
                    if web_enabled && web.is_none() {
                        let _ = request.reply.send(Err(std::io::Error::other("web control drain not owned")));
                        continue;
                    }
                    drop(listener.take());
                    backend.close();
                    let _ = shutdown.send(true);
                    // Use the SAME boot deadline, not a fresh grace window.
                    // A blocked backend cannot strand the signal owner forever.
                    let drain = async {
                        let mut result = client_failure.take().map_or(Ok(()), |e| Err(std::io::Error::other(e)));
                        if let Some(web) = web.take()
                            && let Err(error) = web.shutdown_for_restart().await {
                            result = Err(std::io::Error::other(error));
                        }
                        while let Some(joined) = clients.join_next().await {
                            if let Err(error) = joined { result = Err(std::io::Error::other(error)); }
                        }
                        result
                    };
                    let (result, operator_shutdown) = tokio::select! {
                        result = tokio::time::timeout_at(request.deadline, drain) => (
                            result.unwrap_or_else(|_| Err(std::io::Error::other("control drain exceeded fixed boot deadline"))),
                            false,
                        ),
                        _ = signals.received() => (
                            Err(std::io::Error::other("control drain interrupted by operator shutdown")),
                            true,
                        ),
                    };
                    let result = result
                        .and_then(|()| if Instant::now() >= request.deadline {
                            Err(std::io::Error::other("control drain completed after fixed boot deadline"))
                        } else { Ok(()) });
                    let _ = request.reply.send(result.map(|()| control::ControlDrained { backend: backend.clone() }));
                    if operator_shutdown { break Ok(()); }
                },
                _ = background.notified() => self.idle.reset(),
                _ = area_work.notified() => self.idle.reset(),
                _ = restart.changed() => self.idle.reset(),
                _ = web_activity.changed() => self.idle.reset(),
                _ = expired => {
                    if web_activity.has_changed() {
                        self.idle.reset();
                        continue;
                    }
                    if !(web_enabled && web.is_none()) {
                        let idle_now = || clients.is_empty() && !host.busy()
                            && !self.restart.busy() && !backend.web_delivery_busy() && !web_activity.has_changed();
                        let stop = match web.as_ref() {
                            Some(web) => web.try_begin_idle_shutdown_if(idle_now),
                            None => idle_now(),
                        };
                        if stop { break Ok(()); }
                    }
                },
                event = events.recv() => { if let Ok(event) = event { self.idle.reset(); host.host.lock().unwrap().service_mut().observe_lifecycle(&event); } },
                joined = clients.join_next(), if !clients.is_empty() => {
                    if let Some(Err(error)) = joined { client_failure = Some(error.to_string()); }
                },
                accepted = async { listener.as_ref().unwrap().accept().await }, if listener.is_some() => match accepted {
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
        if let Some(web) = web {
            web.shutdown().await.map_err(std::io::Error::other)?;
        }
        outcome
    }
}
#[cfg(test)]
mod idle;
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
            PreparingDrains,
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
