//! Lifecycle seam for later lane/PR/learning/MCP wiring, not a service catalog.
//! Register only host-owned tasks. Quiesce ingress before signalling them; they
//! must stop producing work and flush before completing. All stop signals are
//! sent before any join, and all tasks are joined even when one fails.
use super::BootstrapError;
use std::future::Future;
use tokio::{sync::oneshot, task::JoinHandle};

struct Service {
    name: &'static str,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), BootstrapError>>,
}

impl Drop for Service {
    fn drop(&mut self) {
        // Emergency teardown only; normal shutdown always awaits completion.
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
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
        self.services.push(Service {
            name,
            stop: Some(stop),
            task: tokio::spawn(factory(stopped)),
        });
    }

    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.services.iter().map(|service| service.name)
    }

    /// Stop producers before draining runtime runs, without waiting for services
    /// that themselves need those runs to reach a terminal state.
    pub fn request_stop(&mut self) {
        for service in &mut self.services {
            if let Some(stop) = service.stop.take() {
                let _ = stop.send(());
            }
        }
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
}
