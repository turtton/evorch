//! Lifecycle seam for later lane/PR/learning/MCP wiring, not a service catalog.
//! Register only host-owned tasks. Quiesce ingress before signalling them; they
//! must stop producing work and flush before completing. All stop signals are
//! sent before any join, and all tasks are joined even when one fails.
use super::BootstrapError;
use std::future::Future;
use tokio::{sync::oneshot, task::JoinHandle};

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ServicePhase {
    Running,
    StopRequested,
    Completed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopMode {
    Operator,
    Planned,
}
enum StopSender {
    Plain(oneshot::Sender<()>),
    Aware(oneshot::Sender<StopMode>),
}
impl StopSender {
    fn send(self, mode: StopMode) {
        match self {
            Self::Plain(sender) => {
                let _ = sender.send(());
            }
            Self::Aware(sender) => {
                let _ = sender.send(mode);
            }
        }
    }
}
struct Service {
    name: &'static str,
    stop: Option<StopSender>,
    task: JoinHandle<Result<(), BootstrapError>>,
    #[cfg(test)]
    phase: tokio::sync::watch::Sender<ServicePhase>,
}

impl Drop for Service {
    fn drop(&mut self) {
        // Emergency teardown only; normal shutdown always awaits completion.
        if let Some(stop) = self.stop.take() {
            stop.send(StopMode::Operator);
        }
        self.task.abort();
    }
}

#[derive(Default)]
pub struct BackgroundServices {
    services: Vec<Service>,
}

impl BackgroundServices {
    /// Creation and stop ownership are paired: a factory receives its own stop
    /// signal and returns the future whose completion releases all its resources.
    pub fn start<F, Fut>(&mut self, name: &'static str, factory: F)
    where
        F: FnOnce(oneshot::Receiver<()>) -> Fut,
        Fut: Future<Output = Result<(), BootstrapError>> + Send + 'static,
    {
        let (stop, stopped) = oneshot::channel();
        self.register(name, StopSender::Plain(stop), factory(stopped));
    }

    /// Services whose operator drain waits for terminal runs need a distinct
    /// planned stop: parked continuations must never be terminated or awaited.
    pub fn start_planned_aware<F, Fut>(&mut self, name: &'static str, factory: F)
    where
        F: FnOnce(oneshot::Receiver<StopMode>) -> Fut,
        Fut: Future<Output = Result<(), BootstrapError>> + Send + 'static,
    {
        let (stop, stopped) = oneshot::channel();
        self.register(name, StopSender::Aware(stop), factory(stopped));
    }
    fn register(
        &mut self,
        name: &'static str,
        stop: StopSender,
        future: impl Future<Output = Result<(), BootstrapError>> + Send + 'static,
    ) {
        #[cfg(test)]
        let (phase, _) = tokio::sync::watch::channel(ServicePhase::Running);
        #[cfg(test)]
        let future = {
            let phase = phase.clone();
            async move {
                let result = future.await;
                phase.send_replace(ServicePhase::Completed);
                result
            }
        };
        self.services.push(Service {
            name,
            stop: Some(stop),
            task: tokio::spawn(future),
            #[cfg(test)]
            phase,
        });
    }

    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.services.iter().map(|service| service.name)
    }

    /// Event-driven lifecycle observation for adversarial shutdown ordering tests.
    #[cfg(test)]
    pub(super) fn lifecycle(
        &self,
        name: &str,
    ) -> Option<tokio::sync::watch::Receiver<ServicePhase>> {
        self.services
            .iter()
            .find(|service| service.name == name)
            .map(|service| service.phase.subscribe())
    }

    /// Stop producers before draining runtime runs, without waiting for services
    /// that themselves need those runs to reach a terminal state.
    pub fn request_stop(&mut self) {
        self.request_stop_mode(StopMode::Operator);
    }

    pub fn request_planned_stop(&mut self) {
        self.request_stop_mode(StopMode::Planned);
    }

    fn request_stop_mode(&mut self, mode: StopMode) {
        for service in &mut self.services {
            if let Some(stop) = service.stop.take() {
                #[cfg(test)]
                service.phase.send_modify(|phase| {
                    if *phase == ServicePhase::Running {
                        *phase = ServicePhase::StopRequested;
                    }
                });
                stop.send(mode);
            }
        }
    }

    /// Join only a producer that must finish before the runtime snapshot.
    /// Other services may still need runtime terminal events to finish.
    pub async fn join_named(&mut self, name: &str) -> Result<(), BootstrapError> {
        let mut selected = Self::default();
        let mut remaining = Vec::new();
        for service in self.services.drain(..) {
            if service.name == name {
                selected.services.push(service);
            } else {
                remaining.push(service);
            }
        }
        self.services = remaining;
        selected.shutdown().await
    }

    pub async fn shutdown(&mut self) -> Result<(), BootstrapError> {
        self.request_stop();
        let mut failure = None;
        for mut service in self.services.drain(..) {
            let result = match (&mut service.task).await {
                Ok(result) => result,
                Err(error) => Err(error.into()),
            };
            if let Err(error) = result {
                let error: BootstrapError = format!("{}: {error}", service.name).into();
                if failure.is_none() {
                    failure = Some(error);
                }
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn planned_stop_is_not_replaced_by_operator_mode_while_joining() {
        let mut services = BackgroundServices::default();
        services.start_planned_aware("parked-learning", |stop| async move {
            assert_eq!(stop.await?, StopMode::Planned);
            Ok(())
        });
        services.request_planned_stop();
        services.request_stop();
        services.shutdown().await.unwrap();
        services.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_signals_every_service_before_joining_and_is_repeatable() {
        let mut services = BackgroundServices::default();
        let (second_stopped, first_may_finish) = oneshot::channel();
        services.start("first", |stop| async move {
            stop.await?;
            first_may_finish.await?;
            Ok(())
        });
        services.start("second", |stop| async move {
            stop.await?;
            second_stopped.send(()).unwrap();
            Ok(())
        });
        assert_eq!(services.names().collect::<Vec<_>>(), ["first", "second"]);
        services.shutdown().await.unwrap();
        assert_eq!(services.names().count(), 0);
        services.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_service_does_not_skip_other_services_drain() {
        let mut services = BackgroundServices::default();
        let (drained, completed) = oneshot::channel();
        services.start("failed", |stop| async move {
            stop.await?;
            Err("service failed".into())
        });
        services.start("healthy", |stop| async move {
            stop.await?;
            drained.send(()).unwrap();
            Ok(())
        });
        assert!(
            services
                .shutdown()
                .await
                .unwrap_err()
                .to_string()
                .starts_with("failed:")
        );
        completed.await.unwrap();
        assert_eq!(services.names().count(), 0);
    }

    #[tokio::test]
    async fn producer_join_does_not_wait_for_a_service_that_needs_runtime_drain() {
        let mut services = BackgroundServices::default();
        let (runtime_drained, drained) = oneshot::channel();
        services.start("runtime-dependent", |stop| async move {
            stop.await?;
            drained.await?;
            Ok(())
        });
        services.start("producer", |stop| async move {
            stop.await?;
            Ok(())
        });
        services.request_stop();
        services.join_named("producer").await.unwrap();
        assert_eq!(services.names().collect::<Vec<_>>(), ["runtime-dependent"]);
        runtime_drained.send(()).unwrap();
        services.shutdown().await.unwrap();
    }
}
