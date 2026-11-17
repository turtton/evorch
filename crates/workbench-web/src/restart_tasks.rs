//! Track detached work independently of HTTP connection/JoinHandle lifetime.
use std::{
    future::Future,
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::task::JoinHandle;
use tokio_util::task::TaskTracker;

/// A restart-only fence and completion proof for detached work.
///
/// Dropping this group or a returned handle does NOT abort accepted commands:
/// ordinary shutdown deliberately has a weaker, connection-only contract.
/// Stop/join all producers before draining this group. Admission is sealed
/// atomically with task registration; panic/abort permanently forbids a proof.
#[derive(Clone, Default)]
pub struct RestartTasks(Arc<Inner>);
#[derive(Default)]
struct Inner {
    tracker: Mutex<TaskTracker>,
    failed: AtomicBool,
    activity: ActivityObserver,
}

struct Completion {
    inner: Arc<Inner>,
    finished: bool,
}
impl Completion {
    fn finish(mut self) {
        self.finished = true;
    }
}
impl Drop for Completion {
    fn drop(&mut self) {
        if !self.finished {
            self.inner.failed.store(true, Ordering::SeqCst);
        }
        self.inner.activity.completed(self.finished);
    }
}
impl RestartTasks {
    pub fn spawn<F>(&self, future: F) -> io::Result<JoinHandle<F::Output>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let tracker = self.0.tracker.lock().unwrap();
        if tracker.is_closed() {
            return Err(io::Error::other("restart task admission closed"));
        }
        // Construct outside the future so abort-before-first-poll also latches.
        let completion = Completion {
            inner: self.0.clone(),
            finished: false,
        };
        self.0.activity.started();
        Ok(tracker.spawn(async move {
            let result = future.await;
            completion.finish();
            result
        }))
    }

    /// Cancellation-safe: dropping the waiter neither loses tracking nor aborts
    /// work. A later waiter observes the same fence and sticky failure latch.
    pub async fn drain(&self) -> io::Result<()> {
        let tracker = {
            let tracker = self.0.tracker.lock().unwrap();
            tracker.close();
            tracker.clone()
        };
        tracker.wait().await;
        if self.0.failed.load(Ordering::SeqCst) {
            Err(io::Error::other("detached work did not finish normally"))
        } else {
            Ok(())
        }
    }
}

/// Read-only, cloneable activity observation independent of task/connection ownership.
/// Snapshots are copied and notification futures never retain an update lock.
#[derive(Clone, Default)]
pub struct ActivityObserver(Arc<Activity>);

// (active items, sticky unconfirmed completion, generation).
// Only copied standard types escape this private lock.
#[derive(Default)]
struct Activity {
    state: Mutex<(usize, bool, u64)>,
    changed: tokio::sync::Notify,
}

impl ActivityObserver {
    fn update(&self, change: impl FnOnce(&mut (usize, bool, u64))) {
        {
            // No user code runs while locked, and poison must not make Drop
            // panic if a preceding task unwound.
            let mut state = self
                .0
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            change(&mut state);
            state.2 = state.2.wrapping_add(1);
        }
        self.0.changed.notify_waiters();
    }

    pub(crate) fn started(&self) {
        self.update(|state| state.0 = state.0.saturating_add(1));
    }

    pub(crate) fn completed(&self, finished: bool) {
        self.update(|state| {
            state.0 = state.0.saturating_sub(1);
            state.1 |= !finished;
        });
    }
}

impl ActivityObserver {
    /// Copied active count, sticky unconfirmed-completion flag and generation.
    /// No update lock escapes this method.
    pub fn snapshot(&self) -> (usize, bool, u64) {
        *self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Await a generation different from the caller's copied snapshot. Register
    /// before rechecking to avoid losing a concurrent update; a completed pair
    /// of start/end transitions also changes the generation. No lock crosses
    /// an await, and cancellation never affects activity ownership.
    pub async fn changed(&self, observed: u64) {
        loop {
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.snapshot().2 != observed {
                return;
            }
            changed.await;
        }
    }
}

impl RestartTasks {
    /// Observe without closing admission or changing task ownership/drain.
    pub fn activity(&self) -> ActivityObserver {
        self.0.activity.clone()
    }
}

#[cfg(test)]
mod activity_tests {
    use super::*;
    use std::task::{Context, Waker};
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn open_admission_reports_increases_decreases_and_coalesced_activity() {
        let tasks = RestartTasks::default();
        let observer = tasks.activity();
        let mut observed = observer.snapshot();
        assert_eq!((observed.0, observed.1), (0, false));
        let (release, receive) = oneshot::channel();
        let first = tasks.spawn(async move { receive.await.unwrap() }).unwrap();
        observer.changed(observed.2).await;
        observed = observer.snapshot();
        assert_eq!((observed.0, observed.1), (1, false));
        let (release_second, receive_second) = oneshot::channel();
        let second = tasks
            .spawn(async move { receive_second.await.unwrap() })
            .unwrap();
        observer.changed(observed.2).await;
        observed = observer.snapshot();
        assert_eq!((observed.0, observed.1), (2, false));

        release.send(()).unwrap();
        first.await.unwrap();
        observer.changed(observed.2).await;
        observed = observer.snapshot();
        assert_eq!((observed.0, observed.1), (1, false));
        release_second.send(()).unwrap();
        second.await.unwrap();
        observer.changed(observed.2).await;
        observed = observer.snapshot();
        assert_eq!((observed.0, observed.1), (0, false));

        // Observation never seals admission; even an entirely completed task
        // since the last observation leaves a generation to reset idle grace.
        tasks.spawn(async {}).unwrap().await.unwrap();
        observer.changed(observed.2).await;
        let after = observer.snapshot();
        assert_ne!(after.2, observed.2);
        assert_eq!((after.0, after.1), (0, false));
        tasks.drain().await.unwrap();
    }

    #[tokio::test]
    async fn aborted_or_panicking_tasks_publish_a_sticky_unconfirmed_completion() {
        for panic in [false, true] {
            let tasks = RestartTasks::default();
            let observer = tasks.activity();
            let observed = observer.snapshot();
            let task = if panic {
                tasks.spawn(async { panic!("injected failure") }).unwrap()
            } else {
                let task = tasks.spawn(std::future::pending::<()>()).unwrap();
                task.abort();
                task
            };
            assert!(task.await.is_err());
            observer.changed(observed.2).await;
            let activity = observer.snapshot();
            assert_eq!((activity.0, activity.1), (0, true));
            assert!(tasks.drain().await.is_err());
            assert_eq!(observer.snapshot(), activity);
        }
    }

    #[test]
    fn observer_snapshot_and_waiter_allow_completion_on_another_thread() {
        struct CompleteOnDrop(ActivityObserver);
        impl Drop for CompleteOnDrop {
            fn drop(&mut self) {
                self.0.completed(false);
            }
        }
        let observer = ActivityObserver::default();
        observer.started();
        let copied = observer.snapshot();
        let mut changed = Box::pin(observer.changed(copied.2));
        let mut context = Context::from_waker(Waker::noop());
        assert!(changed.as_mut().poll(&mut context).is_pending());
        let completion = CompleteOnDrop(observer.clone());
        std::thread::spawn(move || drop(completion)).join().unwrap();
        assert_eq!((copied.0, copied.1), (1, false));
        assert!(changed.as_mut().poll(&mut context).is_ready());
        let after = observer.snapshot();
        assert_eq!((after.0, after.1), (0, true));
        assert_ne!(after.2, copied.2);
    }

    #[test]
    fn held_observation_and_waiter_do_not_block_runtime_destruction() {
        let tasks = RestartTasks::default();
        let observer = tasks.activity();
        let owner = tasks.clone();
        let (started, entered) = std::sync::mpsc::channel();
        let (release, receive) = oneshot::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                drop(
                    owner
                        .spawn(async move {
                            started.send(()).unwrap();
                            std::future::pending::<()>().await;
                        })
                        .unwrap(),
                );
                receive.await.unwrap();
            });
            // Aborts the still-live task and runs Completion::drop.
            drop(runtime);
        });
        entered.recv().unwrap();
        let observation = observer.snapshot();
        assert_eq!((observation.0, observation.1), (1, false));
        let mut changed = Box::pin(observer.changed(observation.2));
        let mut context = Context::from_waker(Waker::noop());
        assert!(changed.as_mut().poll(&mut context).is_pending());
        release.send(()).unwrap();
        // Intentionally retain both the copied observation and a registered
        // notification future across destruction. No timeout/sleep: the test
        // runner detects a regression that deadlocks this separate thread.
        thread.join().unwrap();
        assert_eq!((observation.0, observation.1), (1, false));
        assert!(changed.as_mut().poll(&mut context).is_ready());
        let after = observer.snapshot();
        assert_eq!((after.0, after.1), (0, true));
        assert_ne!(after.2, observation.2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{future::poll_fn, task::Poll};
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn disconnect_and_cancelled_waiter_keep_work_owned_and_sealed() {
        let tasks = RestartTasks::default();
        let (release, receive) = oneshot::channel();
        let (done, finished) = oneshot::channel();
        drop(
            tasks
                .spawn(async move {
                    receive.await.unwrap();
                    done.send(()).unwrap();
                })
                .unwrap(),
        );
        {
            let mut wait = Box::pin(tasks.drain());
            assert!(
                poll_fn(|cx| Poll::Ready(wait.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );
        }
        assert!(tasks.spawn(async {}).is_err());
        release.send(()).unwrap();
        tasks.drain().await.unwrap();
        finished.await.unwrap();
        tasks.drain().await.unwrap();
    }

    #[tokio::test]
    async fn panic_and_abort_are_sticky_unconfirmed_drains() {
        for panic in [false, true] {
            let tasks = RestartTasks::default();
            let handle = if panic {
                tasks
                    .spawn(async { panic!("injected detached failure") })
                    .unwrap()
            } else {
                let handle = tasks.spawn(std::future::pending::<()>()).unwrap();
                handle.abort(); // before the first poll
                handle
            };
            assert!(handle.await.is_err());
            assert!(tasks.drain().await.is_err());
            assert!(tasks.drain().await.is_err());
            assert!(tasks.spawn(async {}).is_err());
        }
    }
}
