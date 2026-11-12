//! Tasks an area service owns through the host.
//!
//! An area's command and query handlers are synchronous and run while the
//! host's mutex is held, so they cannot await. Work that has to wait (a
//! network call, a child process, a long scan) is handed to [`AreaTasks`]
//! instead: it is started on the host's runtime, counted against a fixed limit,
//! told to stop on shutdown and joined, all without the caller holding or
//! awaiting anything. There is no other way to start a background task from an
//! area: a detached `tokio::spawn` would outlive the shutdown that drains the
//! runtime and storage.
use crate::bootstrap::BootstrapError;
use std::{
    future::Future,
    sync::{Arc, Mutex, PoisonError},
};
use tokio::{sync::watch, task::JoinHandle};

/// How many tasks areas may have running at once, together.
pub const MAX_AREA_TASKS: usize = 64;

/// Why a task was not started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SpawnError {
    /// The host is shutting down; nothing new starts.
    #[error("the host is shutting down")]
    Closed,
    /// [`MAX_AREA_TASKS`] tasks are running.
    #[error("too many area tasks are running")]
    Full,
}

/// Resolves when the host asks the task to stop. A task must then finish what
/// it holds (flush, release, join its own children) and return.
pub struct StopSignal(watch::Receiver<bool>);

impl StopSignal {
    pub async fn stopped(&mut self) {
        // A dropped sender means the registry is gone: also a stop.
        let _ = self.0.wait_for(|stop| *stop).await;
    }

    pub fn is_stopped(&self) -> bool {
        *self.0.borrow()
    }
}

struct Entry {
    task: JoinHandle<()>,
}

#[derive(Default)]
struct State {
    closed: bool,
    tasks: Vec<Entry>,
    /// What tasks that already ended reported; handed out by `shutdown`.
    failures: Vec<String>,
}

struct Inner {
    runtime: tokio::runtime::Handle,
    stop: watch::Sender<bool>,
    state: Mutex<State>,
}

impl Inner {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Emergency teardown only (a failed start-up); a normal stop always
        // goes through `AreaTasks::shutdown`, which joins.
        self.stop.send_replace(true);
        for entry in std::mem::take(&mut self.state().tasks) {
            entry.task.abort();
        }
    }
}

/// The registry of area tasks; cloning shares it.
#[derive(Clone)]
pub struct AreaTasks(Arc<Inner>);

impl AreaTasks {
    /// A registry whose tasks run on `runtime`.
    pub fn new(runtime: tokio::runtime::Handle) -> Self {
        Self(Arc::new(Inner {
            runtime,
            stop: watch::channel(false).0,
            state: Mutex::default(),
        }))
    }

    /// Start `name`. `task` receives its stop signal and returns the future
    /// whose completion releases everything the task holds; an error it
    /// returns (or a panic) is reported by [`Self::shutdown`].
    ///
    /// # Errors
    /// [`SpawnError::Closed`] once a stop was requested, [`SpawnError::Full`]
    /// at the limit.
    pub fn spawn<F, Fut>(&self, name: &'static str, task: F) -> Result<(), SpawnError>
    where
        F: FnOnce(StopSignal) -> Fut,
        Fut: Future<Output = Result<(), BootstrapError>> + Send + 'static,
    {
        let mut state = self.0.state();
        if state.closed {
            return Err(SpawnError::Closed);
        }
        state.tasks.retain(|entry| !entry.task.is_finished());
        if state.tasks.len() >= MAX_AREA_TASKS {
            return Err(SpawnError::Full);
        }
        let future = task(StopSignal(self.0.stop.subscribe()));
        let inner = self.0.runtime.spawn(future);
        let shared = Arc::downgrade(&self.0);
        // Supervising task: records how `name` ended, so a failure that
        // happens before shutdown is not lost when the entry is pruned.
        let supervisor = self.0.runtime.spawn(async move {
            let failure = match inner.await {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(format!("{name}: {error}")),
                Err(error) => Some(format!("{name}: {error}")),
            };
            if let (Some(failure), Some(shared)) = (failure, shared.upgrade()) {
                shared.state().failures.push(failure);
            }
        });
        state.tasks.push(Entry { task: supervisor });
        Ok(())
    }

    /// Tasks that have not ended.
    pub fn active(&self) -> usize {
        self.0
            .state()
            .tasks
            .iter()
            .filter(|entry| !entry.task.is_finished())
            .count()
    }

    /// Refuse new tasks and tell every running one to stop, without waiting.
    /// Call it before draining what the tasks depend on, so they stop
    /// producing work first; [`Self::shutdown`] then joins them.
    pub fn request_stop(&self) {
        self.0.state().closed = true;
        self.0.stop.send_replace(true);
    }

    /// Stop every task and join all of them, even when one failed. Reports the
    /// first failure, including those of tasks that ended earlier. Repeatable.
    ///
    /// # Errors
    /// The first failure any task reported.
    pub async fn shutdown(&self) -> Result<(), BootstrapError> {
        self.request_stop();
        // Take the handles out so the lock is never held across an await.
        let tasks = std::mem::take(&mut self.0.state().tasks);
        for entry in tasks {
            // The supervisor never fails; a join error is only a cancellation.
            let _ = entry.task.await;
        }
        let mut failures = std::mem::take(&mut self.0.state().failures).into_iter();
        failures.next().map_or(Ok(()), |first| Err(first.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    fn tasks() -> AreaTasks {
        AreaTasks::new(tokio::runtime::Handle::current())
    }

    #[tokio::test]
    async fn shutdown_signals_stop_and_joins_every_task() {
        let tasks = tasks();
        let (drained_a, a) = oneshot::channel();
        let (drained_b, b) = oneshot::channel();
        for (name, drained) in [("a", drained_a), ("b", drained_b)] {
            tasks
                .spawn(name, |mut stop| async move {
                    stop.stopped().await;
                    drained.send(()).unwrap();
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(tasks.active(), 2);
        tasks.shutdown().await.unwrap();
        // Both ran their drain to completion before shutdown returned.
        a.await.unwrap();
        b.await.unwrap();
        assert_eq!(tasks.active(), 0);
        tasks.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn nothing_starts_after_a_stop_was_requested() {
        let tasks = tasks();
        tasks.request_stop();
        let refused = tasks.spawn("late", |_| async { Ok(()) });
        assert_eq!(refused, Err(SpawnError::Closed));
    }

    #[tokio::test]
    async fn the_number_of_running_tasks_is_bounded() {
        let tasks = tasks();
        for _ in 0..MAX_AREA_TASKS {
            tasks
                .spawn("held", |mut stop| async move {
                    stop.stopped().await;
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(
            tasks.spawn("one-more", |_| async { Ok(()) }),
            Err(SpawnError::Full)
        );
        tasks.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_failure_is_reported_and_does_not_skip_the_other_tasks_drain() {
        let tasks = tasks();
        let (drained, completed) = oneshot::channel();
        tasks
            .spawn("failing", |mut stop| async move {
                stop.stopped().await;
                Err("it broke".into())
            })
            .unwrap();
        tasks
            .spawn("healthy", |mut stop| async move {
                stop.stopped().await;
                drained.send(()).unwrap();
                Ok(())
            })
            .unwrap();
        let error = tasks.shutdown().await.unwrap_err().to_string();
        assert_eq!(error, "failing: it broke");
        completed.await.unwrap();
    }

    #[tokio::test]
    async fn a_task_that_failed_before_shutdown_is_still_reported() {
        let tasks = tasks();
        let (ended, failed) = oneshot::channel();
        tasks
            .spawn("early", |_| async move {
                let _ = ended.send(());
                Err("early failure".into())
            })
            .unwrap();
        failed.await.unwrap();
        let error = tasks.shutdown().await.unwrap_err().to_string();
        assert_eq!(error, "early: early failure");
    }
}
