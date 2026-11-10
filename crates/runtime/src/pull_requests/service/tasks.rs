//! Join ownership for every PR producer, including tasks spawned by producers.
use std::{future::Future, sync::Mutex};
use tokio::task::{JoinError, JoinSet};

#[cfg(test)]
mod tests;

#[derive(Default)]
pub(super) struct ServiceTasks {
    state: Mutex<TaskState>,
    draining: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct TaskState {
    tasks: JoinSet<()>,
    failure: Option<JoinError>,
}

impl ServiceTasks {
    pub(super) fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A monitor lives indefinitely; do not retain every completed fetch.
        while let Some(result) = state.tasks.try_join_next() {
            if let Err(error) = result {
                tracing::warn!(%error, "pull request task failed");
                state.failure.get_or_insert(error);
            }
        }
        state.tasks.spawn(task);
    }

    pub(super) async fn drain(&self) -> Result<(), JoinError> {
        let _draining = self.draining.lock().await;
        let mut failure = None;
        loop {
            let mut batch = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(error) = state.failure.take() {
                    failure.get_or_insert(error);
                }
                std::mem::take(&mut state.tasks)
            };
            if batch.is_empty() {
                break;
            }
            while let Some(result) = batch.join_next().await {
                if let Err(error) = result {
                    failure.get_or_insert(error);
                }
            }
            // A joined producer may have registered children while it was draining.
        }
        failure.map_or(Ok(()), Err)
    }
}
