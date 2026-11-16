//! Admission and child supervision share one lock, so shutdown cannot miss a
//! child created by an in-flight tool. The host must quiesce runtime ingress first.
use std::sync::{Arc, Mutex};
use tokio::{sync::watch, task::JoinHandle};

#[derive(Default)]
pub(super) struct State {
    pub closed: bool,
    pub tasks: Vec<JoinHandle<()>>,
    drain_failure: Option<tokio::task::JoinError>,
}

impl Drop for State {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[derive(Clone)]
pub struct EvaluationService {
    pub(super) state: Arc<Mutex<State>>,
    pub(super) stop: watch::Sender<bool>,
    pub producers: runtime::producer::ProducerGate,
    drained: watch::Sender<bool>,
    shutdown_lock: Arc<tokio::sync::Mutex<()>>,
    ledger: Arc<Mutex<Option<sandbox::SpawnLedger>>>,
}

impl Default for EvaluationService {
    fn default() -> Self {
        Self {
            state: Arc::default(),
            stop: watch::channel(false).0,
            producers: runtime::producer::ProducerGate::default(),
            drained: watch::channel(false).0,
            shutdown_lock: Arc::default(),
            ledger: Arc::default(),
        }
    }
}

impl EvaluationService {
    /// Admission must already be paused and drained. Cancellation drops only the
    /// observer: unfinished JoinHandles remain owned by the service.
    pub async fn drain_accepted(&self) -> Result<(), String> {
        use std::{future::Future, pin::Pin, task::Poll};
        let _shutdown = self.shutdown_lock.lock().await;
        std::future::poll_fn(|cx| {
            let mut state = self.state.lock().unwrap();
            let mut index = 0;
            while index < state.tasks.len() {
                match Pin::new(&mut state.tasks[index]).poll(cx) {
                    Poll::Pending => index += 1,
                    Poll::Ready(result) => {
                        drop(state.tasks.swap_remove(index));
                        if let Err(error) = result {
                            state.drain_failure.get_or_insert(error);
                        }
                    }
                }
            }
            if let Some(error) = &state.drain_failure {
                Poll::Ready(Err(error.to_string()))
            } else if state.tasks.is_empty() {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        })
        .await
    }

    /// Register every evaluation child this service starts in the core's
    /// ledger. Call it before the first evaluation starts; once the ledger's
    /// spawn fence is closed no evaluation starts.
    #[must_use]
    pub fn with_spawn_ledger(self, ledger: sandbox::SpawnLedger) -> Self {
        *self
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ledger);
        self
    }

    pub(super) fn spawn_ledger(&self) -> Option<sandbox::SpawnLedger> {
        self.ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Register host work while admission is open; shutdown owns its completion.
    pub fn spawn(
        &self,
        work: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<(), &'static str> {
        let _producer = self
            .producers
            .try_enter()
            .ok_or("evaluation host is busy or stopping")?;
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Err("evaluation host is stopping");
        }
        state.tasks.push(tokio::spawn(work));
        Ok(())
    }

    /// Child execution and its completion/report/notice future are all work.
    pub fn is_busy(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.tasks.iter().any(|task| !task.is_finished())
            || (state.closed && !*self.drained.borrow())
    }

    pub fn request_stop(&self) {
        self.producers.close();
        self.state.lock().unwrap().closed = true;
        self.stop.send_replace(true);
    }

    /// One lifecycle owner calls this; all child waits and notice writes finish.
    pub async fn shutdown(&self) -> Result<(), tokio::task::JoinError> {
        let _shutdown = self.shutdown_lock.lock().await;
        self.request_stop();
        let (tasks, mut failure) = {
            let mut state = self.state.lock().unwrap();
            (std::mem::take(&mut state.tasks), state.drain_failure.take())
        };
        for task in tasks {
            if let Err(error) = task.await {
                failure.get_or_insert(error);
            }
        }
        self.drained.send_replace(true);
        failure.map_or(Ok(()), Err)
    }

    pub async fn wait_drained(&self) {
        let mut drained = self.drained.subscribe();
        while !*drained.borrow_and_update() {
            if drained.changed().await.is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod restart_drain_tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_observer_retains_running_evaluation_without_stop() {
        let service = EvaluationService::default();
        let (release, returned) = tokio::sync::oneshot::channel();
        service.spawn(async { returned.await.unwrap() }).unwrap();
        let pause = service.producers.pause();
        let mut observer = Box::pin(service.drain_accepted());
        assert!(
            std::future::poll_fn(|cx| {
                std::task::Poll::Ready(
                    std::future::Future::poll(observer.as_mut(), cx).is_pending(),
                )
            })
            .await
        );
        drop(observer);
        assert!(service.is_busy());
        assert!(!*service.stop.borrow());
        release.send(()).unwrap();
        service.drain_accepted().await.unwrap();
        assert!(!service.is_busy());
        assert!(service.producers.resume(pause));
        service.spawn(async {}).unwrap();
        service.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn natural_drain_failure_is_still_reported_by_shutdown() {
        let service = EvaluationService::default();
        let mut stopped = service.stop.subscribe();
        service
            .spawn(async move { stopped.changed().await.unwrap() })
            .unwrap();
        service.spawn(async { panic!("evaluation fault") }).unwrap();
        service.producers.pause();
        assert!(service.drain_accepted().await.is_err());
        assert!(service.is_busy());
        assert!(!*service.stop.borrow());
        assert!(service.shutdown().await.is_err());
    }
}
