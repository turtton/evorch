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
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    sync::{Notify, watch},
    task::JoinHandle,
};

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

/// A task that is aborted when this handle is dropped, so the supervisor that
/// owns it can never leave it running when the supervisor itself is aborted.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Future for AbortOnDrop<T> {
    type Output = Result<T, tokio::task::JoinError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.0).poll(cx)
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
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
    producers: runtime::producer::ProducerGate,
    stop: watch::Sender<bool>,
    state: Mutex<State>,
    /// Tasks started with [`AreaTasks::spawn_counted`] that have not ended.
    counted: Arc<AtomicUsize>,
    /// Woken each time a counted task ends, so an idle check runs again.
    counted_ended: Arc<Notify>,
}

impl Inner {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Emergency teardown only (a failed start-up); a normal stop always
        // goes through `AreaTasks::shutdown`, which joins. Aborting a
        // supervisor drops the task it owns, so nothing is left running.
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
            producers: Default::default(),
            stop: watch::channel(false).0,
            state: Mutex::default(),
            counted: Arc::new(AtomicUsize::new(0)),
            counted_ended: Arc::new(Notify::new()),
        }))
    }

    /// [`Self::spawn`] for work that keeps the host from stopping on idle:
    /// while the task runs, [`Self::busy`] is true, and `counted_ended_notify`
    /// is woken when it ends. A long-lived watcher must use plain `spawn`.
    ///
    /// # Errors
    /// As [`Self::spawn`].
    pub fn spawn_counted<F, Fut>(&self, name: &'static str, task: F) -> Result<(), SpawnError>
    where
        F: FnOnce(StopSignal) -> Fut,
        Fut: Future<Output = Result<(), BootstrapError>> + Send + 'static,
    {
        self.spawn_counted_tracked(name, None, task)
    }

    /// An ingress-accepted operation must finish even when producers are paused.
    /// Its original admission guard (never a fresh admission) stays through persistence.
    pub(crate) fn spawn_counted_tracked<F, Fut>(
        &self,
        name: &'static str,
        accepted: Option<crate::bootstrap::restart::HostCommandPermit>,
        task: F,
    ) -> Result<(), SpawnError>
    where
        F: FnOnce(StopSignal) -> Fut,
        Fut: Future<Output = Result<(), BootstrapError>> + Send + 'static,
    {
        self.spawn_inner(name, task, true, accepted)
    }

    /// Admission of new counted operations; passive area watchers remain running.
    pub fn producer_gate(&self) -> runtime::producer::ProducerGate {
        self.0.producers.clone()
    }

    /// Whether accepted counted work is still pending or running.
    pub fn busy(&self) -> bool {
        self.0.counted.load(Ordering::SeqCst) != 0
    }

    /// Woken each time a counted task ends (it keeps one permit when nobody
    /// waits), so the owner of the idle decision looks again.
    pub fn counted_ended_notify(&self) -> Arc<Notify> {
        self.0.counted_ended.clone()
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
        self.spawn_inner(name, task, false, None)
    }

    fn spawn_inner<F, Fut>(
        &self,
        name: &'static str,
        task: F,
        counted: bool,
        accepted: Option<crate::bootstrap::restart::HostCommandPermit>,
    ) -> Result<(), SpawnError>
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
        let producers = self.0.producers.clone();
        let mut stop = StopSignal(self.0.stop.subscribe());
        let future = async move {
            let _accepted = accepted;
            let _permit = if counted && _accepted.is_none() {
                tokio::select! { biased; _ = stop.stopped() => return Ok(()), permit = producers.enter() => {
                    let Some(permit) = permit else { return Ok(()); };
                    Some(permit)
                }}
            } else {
                None
            };
            future.await
        };
        // The task is owned by its supervisor (and aborted with it), so the
        // registry's own handle is the only one there is to stop.
        let inner = AbortOnDrop(self.0.runtime.spawn(future));
        let shared = Arc::downgrade(&self.0);
        let counter = counted.then(|| {
            self.0.counted.fetch_add(1, Ordering::SeqCst);
            (self.0.counted.clone(), self.0.counted_ended.clone())
        });
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
            if let Some((counted, ended)) = counter {
                counted.fetch_sub(1, Ordering::SeqCst);
                ended.notify_one();
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
        self.0.producers.close();
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

    #[tokio::test]
    async fn paused_counted_work_starts_once_after_resume_but_never_after_shutdown() {
        let tasks = tasks();
        let gate = tasks.producer_gate();
        let pause = gate.pause();
        let starts = Arc::new(AtomicUsize::new(0));
        let count = starts.clone();
        let (started, observed) = oneshot::channel();
        tasks
            .spawn_counted("pending", move |_| async move {
                count.fetch_add(1, Ordering::SeqCst);
                started.send(()).unwrap();
                Ok(())
            })
            .unwrap();
        assert!(
            tasks.busy(),
            "accepted pending work remains owned by the host"
        );
        assert!(gate.resume(pause));
        observed.await.unwrap();
        tasks.shutdown().await.unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(!gate.resume(pause));

        let stopped = AreaTasks::new(tokio::runtime::Handle::current());
        stopped.producer_gate().pause();
        let count = starts.clone();
        stopped
            .spawn_counted("never-start", move |_| async move {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .unwrap();
        stopped.shutdown().await.unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(!stopped.busy());
    }

    #[tokio::test]
    async fn ingress_accepted_unpolled_work_bypasses_pause_but_holds_drain_until_finished() {
        use crate::bootstrap::restart::{CommandAccess, HostRestartGate, HostRestartPhase};
        let tasks = tasks();
        let host = HostRestartGate::default();
        let accepted = host.enter(CommandAccess::NewWork).unwrap();
        let synchronous_reply = accepted.clone();
        let (started, observed) = oneshot::channel();
        let (release, released) = oneshot::channel();
        tasks
            .spawn_counted_tracked("accepted", Some(accepted), move |_| async move {
                started.send(()).unwrap();
                released.await.unwrap();
                Ok(())
            })
            .unwrap();
        // No await: the accepted task has not been polled before the pause.
        tasks.producer_gate().pause();
        host.transition(1, HostRestartPhase::Sealing);
        assert!(host.enter(CommandAccess::NewWork).is_err());
        observed.await.unwrap();
        let mut drained = Box::pin(host.drained());
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                drained.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        release.send(()).unwrap();
        tasks.shutdown().await.unwrap();
        // Even if async persistence finishes first, the original reply still owns admission.
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                drained.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        drop(synchronous_reply);
        drained.await;
        assert!(!tasks.busy());
    }

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

    /// Tells the test when the future holding it is dropped.
    struct DropSignal(Option<oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            let _ = self.0.take().unwrap().send(());
        }
    }

    #[tokio::test]
    async fn dropping_the_registry_drops_the_real_tasks_too() {
        let tasks = tasks();
        let (dropped, task_dropped) = oneshot::channel();
        tasks
            .spawn("held", |mut stop| async move {
                let _held = DropSignal(Some(dropped));
                // Never returns on its own: only the teardown can end it.
                std::future::pending::<()>().await;
                stop.stopped().await;
                Ok(())
            })
            .unwrap();
        drop(tasks);
        // The future itself was dropped (aborted), not only a supervisor.
        task_dropped.await.unwrap();
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

    #[tokio::test]
    async fn only_a_counted_task_keeps_the_host_busy_and_its_end_wakes_the_idle_check() {
        let tasks = tasks();
        let ended = tasks.counted_ended_notify();
        let (finish, finished) = oneshot::channel::<()>();
        tasks
            .spawn("watcher", |mut stop| async move {
                stop.stopped().await;
                Ok(())
            })
            .unwrap();
        assert!(!tasks.busy(), "a watcher is not work");
        tasks
            .spawn_counted("operation", |_| async move {
                let _ = finished.await;
                Ok(())
            })
            .unwrap();
        assert!(tasks.busy());
        finish.send(()).unwrap();
        ended.notified().await;
        assert!(!tasks.busy(), "the wake-up comes after the count dropped");
        tasks.shutdown().await.unwrap();
    }
}
