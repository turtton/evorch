//! Admission and child supervision share one lock, so shutdown cannot miss a
//! child created by an in-flight tool. The host must quiesce runtime ingress first.
use std::sync::{Arc, Mutex};
use tokio::{sync::watch, task::JoinHandle};

#[derive(Default)]
pub(super) struct State {
    pub closed: bool,
    pub tasks: Vec<JoinHandle<()>>,
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
    drained: watch::Sender<bool>,
    shutdown_lock: Arc<tokio::sync::Mutex<()>>,
}

impl Default for EvaluationService {
    fn default() -> Self {
        Self {
            state: Arc::default(),
            stop: watch::channel(false).0,
            drained: watch::channel(false).0,
            shutdown_lock: Arc::default(),
        }
    }
}

impl EvaluationService {
    /// Register host work while admission is open; shutdown owns its completion.
    pub fn spawn(
        &self,
        work: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<(), &'static str> {
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
        self.state.lock().unwrap().closed = true;
        self.stop.send_replace(true);
    }

    /// One lifecycle owner calls this; all child waits and notice writes finish.
    pub async fn shutdown(&self) -> Result<(), tokio::task::JoinError> {
        let _shutdown = self.shutdown_lock.lock().await;
        self.request_stop();
        let tasks = std::mem::take(&mut self.state.lock().unwrap().tasks);
        let mut failure = None;
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
