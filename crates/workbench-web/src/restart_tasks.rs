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
