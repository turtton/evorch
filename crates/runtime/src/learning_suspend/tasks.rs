//! Finite learning tails are distinct from the long-lived admission scheduler.
//! Join errors survive pruning; dropping a drain returns every unjoined handle.
use super::*;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};
use tokio::task::JoinHandle;
type Task = JoinHandle<Result<(), String>>;

#[derive(Default)]
pub(crate) struct Tasks {
    state: Mutex<TaskState>,
    drain: tokio::sync::Mutex<()>,
}
#[derive(Default)]
struct TaskState {
    finite: Vec<Task>,
    scheduler: Option<Task>,
    failure: Option<String>,
    sealed: bool,
}
impl TaskState {
    fn record(&mut self, result: Result<Result<(), String>, tokio::task::JoinError>) {
        let error = match result {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(_) => Some("learning owned task did not return normally".into()),
        };
        if let Some(error) = error {
            self.failure.get_or_insert(error);
        }
    }
    fn prune(&mut self) {
        let mut remaining = Vec::new();
        for mut task in std::mem::take(&mut self.finite) {
            if task.is_finished() {
                match Pin::new(&mut task).poll(&mut Context::from_waker(Waker::noop())) {
                    Poll::Ready(result) => self.record(result),
                    Poll::Pending => remaining.push(task),
                }
            } else {
                remaining.push(task);
            }
        }
        self.finite = remaining;
    }
}
struct Batch<'a> {
    owner: &'a Tasks,
    handles: Vec<Task>,
}
impl Drop for Batch<'_> {
    fn drop(&mut self) {
        self.owner
            .state
            .lock()
            .unwrap()
            .finite
            .append(&mut self.handles);
    }
}
impl Tasks {
    pub(crate) fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) -> bool {
        self.spawn_result(async move {
            future.await;
            Ok(())
        })
    }
    pub(crate) fn spawn_result(
        &self,
        future: impl Future<Output = Result<(), String>> + Send + 'static,
    ) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.sealed {
            return false;
        }
        state.prune();
        state.finite.push(tokio::spawn(future));
        true
    }
    pub(super) fn scheduler(&self, future: impl Future<Output = ()> + Send + 'static) {
        let mut state = self.state.lock().unwrap();
        if !state.sealed && state.scheduler.is_none() {
            state.scheduler = Some(tokio::spawn(async move {
                future.await;
                Ok(())
            }));
        }
    }
    pub(super) async fn drain(&self) -> Result<(), String> {
        let _drain = self.drain.lock().await;
        loop {
            let mut batch = Batch {
                owner: self,
                handles: {
                    let mut state = self.state.lock().unwrap();
                    state.prune();
                    if state.finite.is_empty() {
                        return state.failure.clone().map_or(Ok(()), Err);
                    }
                    std::mem::take(&mut state.finite)
                },
            };
            while let Some(task) = batch.handles.last_mut() {
                let result = task.await;
                batch.handles.pop();
                self.state.lock().unwrap().record(result);
            }
        }
    }
    pub(super) fn seal(&self) -> Result<(), String> {
        // A concurrent drain owns handles outside TaskState; empty is not joined.
        let _drain = self
            .drain
            .try_lock()
            .map_err(|_| "learning registry drain is still active".to_string())?;
        let mut state = self.state.lock().unwrap();
        state.prune();
        if state.failure.is_some() || !state.finite.is_empty() || state.sealed {
            return Err("learning registry has not drained".into());
        }
        state.sealed = true;
        Ok(())
    }
    pub(super) async fn join_scheduler(&self) -> Result<(), String> {
        let _drain = self.drain.lock().await;
        // Batch returns the scheduler handle to owned finite tails on cancellation.
        let mut batch = Batch {
            owner: self,
            handles: self
                .state
                .lock()
                .unwrap()
                .scheduler
                .take()
                .into_iter()
                .collect(),
        };
        if let Some(task) = batch.handles.last_mut() {
            let result = task.await;
            batch.handles.pop();
            self.state.lock().unwrap().record(result);
        }
        drop(batch);
        // A cancelled previous join may have returned the handle to finite tails.
        drop(_drain);
        self.drain().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;
    #[tokio::test]
    async fn dropped_drain_retains_tails_and_capture_refuses_unjoined_registry() {
        let tasks = Tasks::default();
        let (release, blocked) = oneshot::channel();
        let (returned, observed) = oneshot::channel();
        assert!(tasks.spawn(async move {
            blocked.await.unwrap();
            returned.send(()).unwrap();
        }));
        let mut drain = Box::pin(tasks.drain());
        assert!(std::future::poll_fn(|cx| Poll::Ready(drain.as_mut().poll(cx).is_pending())).await);
        assert!(
            tasks.seal().is_err(),
            "handles held by a live drain are not joined"
        );
        drop(drain);
        assert!(tasks.seal().is_err());
        release.send(()).unwrap();
        observed.await.unwrap();
        tasks.drain().await.unwrap();
        tasks.seal().unwrap();
        assert!(!tasks.spawn(async {
            panic!("must never start after capture");
        }));
    }
    #[tokio::test]
    async fn owned_result_failure_remains_visible_after_later_successful_tails() {
        let tasks = Tasks::default();
        let (failed, observed) = oneshot::channel();
        assert!(tasks.spawn_result(async move {
            failed.send(()).unwrap();
            Err("pending scan readback unconfirmed".into())
        }));
        observed.await.unwrap();
        let (release, blocked) = oneshot::channel();
        assert!(tasks.spawn(async move {
            blocked.await.unwrap();
        }));
        let mut drain = Box::pin(tasks.drain());
        // A failure must be reported, but does not make other owned tails joined.
        assert!(std::future::poll_fn(|cx| Poll::Ready(drain.as_mut().poll(cx).is_pending())).await);
        release.send(()).unwrap();
        assert_eq!(
            drain.await.unwrap_err(),
            "pending scan readback unconfirmed"
        );
        assert!(tasks.spawn(async {}));
        assert_eq!(
            tasks.drain().await.unwrap_err(),
            "pending scan readback unconfirmed"
        );
        assert!(tasks.seal().is_err());
    }

    #[tokio::test]
    async fn pruning_never_forgets_a_join_panic_and_failure_is_sticky() {
        let tasks = Tasks::default();
        let (done, returned) = oneshot::channel::<()>();
        tasks.spawn(async move {
            let _done = done;
            panic!("finite cleanup failed");
        });
        assert!(returned.await.is_err()); // panic dropped the sender
        tasks.spawn(async {}); // used to prune/drop the failed JoinHandle
        assert!(tasks.drain().await.is_err());
        assert!(tasks.drain().await.is_err());
        assert!(tasks.seal().is_err());
    }
    #[tokio::test]
    async fn scheduler_lifetime_is_not_mistaken_for_a_finite_cohort() {
        let tasks = Tasks::default();
        let (stop, stopped) = oneshot::channel();
        tasks.scheduler(async move {
            stopped.await.unwrap();
        });
        tasks.spawn(async {});
        tasks.drain().await.unwrap();
        tasks.seal().unwrap();
        let mut joined = Box::pin(tasks.join_scheduler());
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(joined.as_mut().poll(cx).is_pending())).await
        );
        stop.send(()).unwrap();
        joined.await.unwrap();
    }
}
