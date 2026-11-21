//! Idle grace applies only with no admitted clients and no retained work.
use std::time::Duration;
use tokio::time::Instant;
#[path = "lifecycle/idle/activity.rs"]
mod idle_activity;
#[path = "lifecycle/idle/clock.rs"]
mod idle_clock;
#[path = "lifecycle/idle/renderer.rs"]
mod idle_renderer;

/// Whether a reconnection grace read from the browser leaf is still running.
pub(crate) fn renderer_grace_running(
    grace: workbench_host::areas::browser::RendererReconnectGrace,
) -> bool {
    idle_renderer::deadline(grace, Instant::now()).is_some()
}

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
pub mod shutdown_watchdog;
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
#[cfg(target_os = "linux")]
pub mod finalization;
mod loop_host;
#[cfg(target_os = "linux")]
pub mod product_exit;
use loop_host::LoopHost;
pub struct LifecycleDriver {
    #[cfg(target_os = "linux")]
    watchdog: Option<shutdown_watchdog::Observer>,
    control: control::ControlDrainHandle,
    control_requests: tokio::sync::mpsc::UnboundedReceiver<control::Request>,
    pub restart: crate::restart::RestartCoordinator,
    #[cfg(target_os = "linux")]
    finalization: finalization::FinalizationHandle,
    #[cfg(target_os = "linux")]
    finalization_inbox: Option<finalization::FinalizationInbox>,
    idle: IdleGrace,
    #[cfg(test)]
    drain_barrier: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
}
impl LifecycleDriver {
    pub fn new(host: &workbench_host::bootstrap::BootstrappedHost) -> Self {
        let (send, control_requests) = tokio::sync::mpsc::unbounded_channel();
        let restart = crate::restart::RestartCoordinator::new(host);
        #[cfg(target_os = "linux")]
        let (finalization, inbox) = finalization::channel(restart.clone());
        Self {
            #[cfg(target_os = "linux")]
            watchdog: None,
            control: control::ControlDrainHandle(send),
            control_requests,
            restart,
            #[cfg(target_os = "linux")]
            finalization,
            #[cfg(target_os = "linux")]
            finalization_inbox: Some(inbox),
            #[cfg(test)]
            drain_barrier: None,
            idle: IdleGrace::new(Duration::from_secs(host.config.core.idle_grace_secs)),
        }
    }
    /// Optional outside product entry; product wrappers require all wiring receipts.
    #[cfg(target_os = "linux")]
    pub fn set_shutdown_watchdog(
        &mut self,
        observer: shutdown_watchdog::Observer,
    ) -> std::io::Result<()> {
        self.restart.set_shutdown_watchdog(observer.clone())?;
        observer.installed_lifecycle();
        self.watchdog = Some(observer);
        Ok(())
    }
    pub fn control_drain(&self) -> control::ControlDrainHandle {
        self.control.clone()
    }
    #[cfg(target_os = "linux")]
    pub fn finalization(&self) -> finalization::FinalizationHandle {
        self.finalization.clone()
    }
    /// Product entry exclusively borrows the stack-owned host. Only this loop
    /// can dispatch prepared commands; no storage/probe work is awaited here.
    pub async fn run_owned_with_web(
        &mut self,
        host: &mut workbench_host::bootstrap::BootstrappedHost,
        backend: std::sync::Arc<crate::backend::HostBackend>,
        listener: tokio::net::UnixListener,
        web: Option<workbench_web::WebServer>,
        signals: OperatorSignals,
        #[cfg(target_os = "linux")] context: &product_exit::ProductExitContext,
    ) -> std::io::Result<()> {
        #[cfg(target_os = "linux")]
        let host =
            LoopHost::Owned {
                host,
                context,
                inbox: Some(self.finalization_inbox.take().ok_or_else(|| {
                    std::io::Error::other("finalization receiver already consumed")
                })?),
            };
        #[cfg(not(target_os = "linux"))]
        let host = LoopHost::Shared(host);
        // The inbox guard already lives in this scope if validation fails.
        #[cfg(target_os = "linux")]
        self.validate_signals(&signals)?;
        self.run_loop(
            host,
            backend,
            listener,
            web.is_some(),
            web,
            signals,
            &idle_clock::TokioClock,
        )
        .await
    }
    #[cfg(target_os = "linux")]
    fn validate_signals(&self, signals: &OperatorSignals) -> std::io::Result<()> {
        if let Some(observer) = &self.watchdog {
            signals.set_shutdown_watchdog(observer.clone())?;
            observer.validate_wiring()?;
        }
        Ok(())
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
        web: Option<workbench_web::WebServer>,
        signals: OperatorSignals,
    ) -> std::io::Result<()> {
        // Shared callers cannot retain a live but unserviced receiver.
        #[cfg(target_os = "linux")]
        drop(self.finalization_inbox.take());
        #[cfg(target_os = "linux")]
        self.validate_signals(&signals)?;
        self.run_with_clock(
            host,
            backend,
            listener,
            web_enabled,
            web,
            signals,
            &idle_clock::TokioClock,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn run_with_clock<C: idle_clock::Clock>(
        &mut self,
        host: &workbench_host::bootstrap::BootstrappedHost,
        backend: std::sync::Arc<crate::backend::HostBackend>,
        listener: tokio::net::UnixListener,
        web_enabled: bool,
        web: Option<workbench_web::WebServer>,
        signals: OperatorSignals,
        clock: &C,
    ) -> std::io::Result<()> {
        #[cfg(target_os = "linux")]
        drop(self.finalization_inbox.take());
        self.run_loop(
            LoopHost::Shared(host),
            backend,
            listener,
            web_enabled,
            web,
            signals,
            clock,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn run_loop<C: idle_clock::Clock>(
        &mut self,
        mut host: LoopHost<'_>,
        backend: std::sync::Arc<crate::backend::HostBackend>,
        listener: impl loop_host::Listener,
        web_enabled: bool,
        mut web: Option<workbench_web::WebServer>,
        mut signals: OperatorSignals,
        clock: &C,
    ) -> std::io::Result<()> {
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let mut web_activity = idle_activity::WebActivity::new(web.as_ref(), &backend);
        let mut listener = Some(listener);
        let mut clients = tokio::task::JoinSet::new();
        let mut client_failure = None;
        let mut events = host.get().runtime.event_bus().subscribe();
        let background = host
            .get()
            .host
            .lock()
            .unwrap()
            .service()
            .background_work_notify();
        let area_work = host.get().area_work_notify();
        let index_work = host.get().thread_index_work_notify();
        let adoption = host.get().runtime.input_adoption_notifier();
        let mut restart = self.restart.subscribe();
        let outcome = loop {
            host.get().host.lock().unwrap().service_mut().poll();
            let (web_count, web_busy) = web_activity.observe(&mut self.idle);
            // Retains the process until the browser leaf's deadline; it is not a
            // client and not work, so the idle grace restarts after it.
            let renderer_grace = idle_renderer::current(host.get(), clock.now());
            let deadline = self.idle.update_at(
                clock.now(),
                // The legacy entry point cannot prove an externally owned web
                // listener empty. Production always transfers the actual owner.
                web_enabled && web.is_none(),
                clients.len() + web_count,
                host.get().busy() || self.restart.busy() || web_busy || renderer_grace.is_some(),
            );
            let expired = clock.expired(deadline);
            let grace_over = clock.expired(renderer_grace);
            tokio::select! {
                _ = signals.received() => break Ok(()),
                command = host.recv_finalization() => {
                    #[cfg(target_os = "linux")]
                    if let Some(command) = command
                        && !host.dispatch(command, &self.finalization, &mut signals).await {
                        break Ok(());
                    }
                    #[cfg(not(target_os = "linux"))]
                    let _ = command;
                },
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
                        #[cfg(test)]
                        if let Some((entered, release)) = self.drain_barrier.take() {
                            let _ = entered.send(());
                            let _ = release.await;
                        }
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
                _ = index_work.notified() => (),
                _ = adoption.notified() => self.idle.reset(),
                _ = restart.changed() => self.idle.reset(),
                _ = grace_over, if renderer_grace.is_some() => self.idle.reset(),
                _ = web_activity.changed() => self.idle.reset(),
                _ = expired => {
                    if web_activity.has_changed() {
                        self.idle.reset();
                        continue;
                    }
                    if !(web_enabled && web.is_none()) {
                        let idle_now = || clients.is_empty() && !host.get().busy()
                            && !self.restart.busy() && !backend.web_delivery_busy() && !web_activity.has_changed()
                            && idle_renderer::current(host.get(), clock.now()).is_none();
                        let stop = match web.as_ref() {
                            Some(web) => web.try_begin_idle_shutdown_if(idle_now),
                            None => idle_now(),
                        };
                        if stop { break Ok(()); }
                    }
                },
                event = events.recv() => { if let Ok(event) = event { self.idle.reset(); host.get().host.lock().unwrap().service_mut().observe_lifecycle(&event); } },
                joined = clients.join_next(), if !clients.is_empty() => {
                    if let Some(Err(error)) = joined { client_failure = Some(error.to_string()); }
                },
                accepted = async { listener.as_ref().unwrap().accept().await }, if listener.is_some() => match accepted {
                    Ok((stream, _)) => { if clients.len() < 64 { clients.spawn(crate::unix::connection(stream, backend.clone(), shutdown.subscribe())); } },
                    Err(error) => break Err(error),
                },
            }
        };
        host.close_finalization();
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
