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
    activity: Activity,
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

// (active tasks, sticky unconfirmed completion). Use public standard types so
// this additive API needs no central crate-root export.
struct Activity(tokio::sync::watch::Sender<(usize, bool)>);

impl Default for Activity {
    fn default() -> Self {
        Self(tokio::sync::watch::channel((0, false)).0)
    }
}

impl Activity {
    fn started(&self) {
        self.0.send_modify(|state| {
            state.0 = state.0.saturating_add(1);
        });
    }

    fn completed(&self, finished: bool) {
        // Drop must not panic, even when the task was never polled or unwinds.
        self.0.send_modify(|state| {
            state.0 = state.0.saturating_sub(1);
            state.1 |= !finished;
        });
    }
}

impl RestartTasks {
    /// Active task count and sticky unconfirmed-completion flag. Neither
    /// observation API closes admission or changes drain semantics.
    pub fn activity_snapshot(&self) -> (usize, bool) {
        *self.0.activity.0.borrow()
    }

    /// Subscribe before taking a snapshot to avoid losing a concurrent change.
    /// Watch revisions also preserve a coalesced busy-to-idle transition.
    pub fn subscribe_activity(&self) -> tokio::sync::watch::Receiver<(usize, bool)> {
        self.0.activity.0.subscribe()
    }
}

#[cfg(test)]
mod activity_tests {
    use super::*;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn open_admission_reports_increases_decreases_and_coalesced_activity() {
        let tasks = RestartTasks::default();
        let mut changes = tasks.subscribe_activity();
        assert_eq!(*changes.borrow_and_update(), (0, false));
        let (release, receive) = oneshot::channel();
        let first = tasks.spawn(async move { receive.await.unwrap() }).unwrap();
        changes.changed().await.unwrap();
        assert_eq!(changes.borrow_and_update().0, 1);
        let (release_second, receive_second) = oneshot::channel();
        let second = tasks
            .spawn(async move { receive_second.await.unwrap() })
            .unwrap();
        changes.changed().await.unwrap();
        assert_eq!(changes.borrow_and_update().0, 2);

        release.send(()).unwrap();
        first.await.unwrap();
        changes.changed().await.unwrap();
        assert_eq!(changes.borrow_and_update().0, 1);
        release_second.send(()).unwrap();
        second.await.unwrap();
        changes.changed().await.unwrap();
        assert_eq!(*changes.borrow_and_update(), (0, false));
        assert_eq!(tasks.activity_snapshot().0, 0);

        // Observation never seals admission; even an entirely completed task
        // since the last observation leaves a revision to reset idle grace.
        tasks.spawn(async {}).unwrap().await.unwrap();
        assert!(changes.has_changed().unwrap());
        assert_eq!(*changes.borrow_and_update(), (0, false));
        tasks.drain().await.unwrap();
    }

    #[tokio::test]
    async fn aborted_or_panicking_tasks_publish_a_sticky_unconfirmed_completion() {
        for panic in [false, true] {
            let tasks = RestartTasks::default();
            let mut changes = tasks.subscribe_activity();
            let task = if panic {
                tasks.spawn(async { panic!("injected failure") }).unwrap()
            } else {
                let task = tasks.spawn(std::future::pending::<()>()).unwrap();
                task.abort();
                task
            };
            assert!(task.await.is_err());
            changes.changed().await.unwrap();
            let activity = *changes.borrow_and_update();
            assert_eq!(activity, (0, true));
            assert!(tasks.drain().await.is_err());
            assert_eq!(tasks.activity_snapshot(), activity);
        }
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
