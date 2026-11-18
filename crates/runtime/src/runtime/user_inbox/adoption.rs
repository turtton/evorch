use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// Ownership of consumed input until its execution phase has been published.
/// It owns no inbox lock and also releases on an error, cancellation or unwind.
/// This is execution-local state, never part of the persisted inbox/receipt.
pub(crate) struct InputAdoption(Arc<AtomicUsize>, Arc<tokio::sync::Notify>);

impl Drop for InputAdoption {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
        // Even ignored wakes and Pending polls need a recheck: no phase event
        // necessarily follows an observer seeing their short-lived guard.
        self.1.notify_one();
    }
}

impl super::UserInbox {
    pub(crate) fn with_adoption_notifier(changed: Arc<tokio::sync::Notify>) -> Self {
        Self {
            adoption_changed: changed,
            ..Self::default()
        }
    }

    /// Guard readiness polling before a receiver can remove input. Only a ready
    /// wake transfers ownership to the loop, through handling and publication.
    /// Pending polls release immediately and notify any idle observer.
    pub(crate) async fn adopt_wake<F: std::future::Future>(
        &self,
        future: F,
    ) -> (F::Output, InputAdoption) {
        let mut future = std::pin::pin!(future);
        std::future::poll_fn(|cx| {
            let adoption = self.begin_adoption();
            match future.as_mut().poll(cx) {
                std::task::Poll::Ready(value) => std::task::Poll::Ready((value, adoption)),
                std::task::Poll::Pending => {
                    drop(adoption);
                    #[cfg(any(test, feature = "idle-test-support"))]
                    self.hold_passive_wait_gate();
                    std::task::Poll::Pending
                }
            }
        })
        .await
    }

    pub(super) fn begin_adoption(&self) -> InputAdoption {
        self.adoptions.fetch_add(1, Ordering::AcqRel);
        InputAdoption(self.adoptions.clone(), self.adoption_changed.clone())
    }

    pub(crate) fn adopting(&self) -> bool {
        self.adoptions.load(Ordering::Acquire) != 0
    }
}

#[cfg(any(test, feature = "idle-test-support"))]
#[derive(Clone, Copy, PartialEq, Eq)]
enum GatePoint {
    Claim,
    PublishWake,
    IgnoredWake,
    PassiveWait,
}

#[cfg(any(test, feature = "idle-test-support"))]
pub(super) struct Gate {
    point: GatePoint,
    reached: tokio::sync::oneshot::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

#[cfg(any(test, feature = "idle-test-support"))]
impl super::UserInbox {
    pub(crate) fn gate_next_adoption(
        &self,
        reached: tokio::sync::oneshot::Sender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    ) {
        *self
            .adoption_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Gate {
            point: GatePoint::Claim,
            reached,
            resume,
        });
    }

    pub(super) fn hold_adoption_gate(&self) {
        let gate = self
            .adoption_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(gate) = gate {
            let _ = gate.reached.send(());
            let _ = gate.resume.recv();
        }
    }

    pub(crate) fn gate_next_wake_adoption(
        &self,
        reached: tokio::sync::oneshot::Sender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    ) {
        self.set_wake_gate(GatePoint::PublishWake, reached, resume);
    }

    pub(crate) fn gate_ignored_wake_adoption(
        &self,
        reached: tokio::sync::oneshot::Sender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    ) {
        self.set_wake_gate(GatePoint::IgnoredWake, reached, resume);
    }

    pub(crate) fn gate_passive_wait(
        &self,
        reached: tokio::sync::oneshot::Sender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    ) {
        self.set_wake_gate(GatePoint::PassiveWait, reached, resume);
    }

    fn set_wake_gate(
        &self,
        point: GatePoint,
        reached: tokio::sync::oneshot::Sender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    ) {
        *self
            .wake_adoption_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Gate {
            point,
            reached,
            resume,
        });
    }

    pub(crate) fn hold_wake_adoption_gate(&self) {
        self.hold_wake_gate(GatePoint::PublishWake);
    }

    pub(crate) fn hold_ignored_wake_adoption_gate(&self) {
        self.hold_wake_gate(GatePoint::IgnoredWake);
    }

    fn hold_passive_wait_gate(&self) {
        self.hold_wake_gate(GatePoint::PassiveWait);
    }

    fn hold_wake_gate(&self, point: GatePoint) {
        let gate = self
            .wake_adoption_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take_if(|gate| gate.point == point);
        if let Some(gate) = gate {
            let _ = gate.reached.send(());
            let _ = gate.resume.recv();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::UserInbox;
    use std::sync::Arc;

    #[tokio::test]
    async fn wake_poll_guards_consumption_and_notifies_release_even_without_a_phase() {
        use std::{
            cell::Cell,
            future::Future,
            task::{Context, Poll},
        };
        let inbox = UserInbox::default();
        let ready = Cell::new(false);
        let mut wake = Box::pin(inbox.adopt_wake(std::future::poll_fn(|_| {
            assert!(inbox.adopting(), "guard must precede receiver polling");
            if ready.get() {
                Poll::Ready(42)
            } else {
                Poll::Pending
            }
        })));
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(wake.as_mut().poll(&mut cx).is_pending());
        assert!(
            !inbox.adopting(),
            "a passive waiter cannot retain the process"
        );
        inbox.adoption_changed.notified().await;
        ready.set(true);
        let Poll::Ready((value, guard)) = wake.as_mut().poll(&mut cx) else {
            panic!("ready input must be adopted");
        };
        assert_eq!(value, 42);
        assert!(inbox.adopting(), "handling outlives receiver readiness");
        drop(wake);
        assert!(inbox.adopting());
        drop(guard);
        assert!(!inbox.adopting());
        // Release before registering an observer must leave a stored permit.
        inbox.adoption_changed.notified().await;
    }

    #[tokio::test]
    async fn panicking_wake_poll_releases_and_notifies() {
        use std::{future::Future, task::Context};
        let inbox = UserInbox::default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut wake = Box::pin(inbox.adopt_wake(std::future::poll_fn::<(), _>(|_| {
                assert!(inbox.adopting());
                panic!("receiver failed");
            })));
            wake.as_mut()
                .poll(&mut Context::from_waker(std::task::Waker::noop()))
        }));
        assert!(result.is_err());
        assert!(!inbox.adopting());
        inbox.adoption_changed.notified().await;
    }

    fn enqueue(inbox: &UserInbox) {
        inbox.enqueue(None, |_| Ok::<_, ()>(())).unwrap();
    }

    #[test]
    fn consumed_inputs_remain_active_until_all_guards_are_dropped() {
        let inbox = UserInbox::default();
        enqueue(&inbox);
        enqueue(&inbox);
        let first = inbox.claim_for_adoption(1).unwrap();
        let second = inbox.claim_for_adoption(2).unwrap();
        assert_eq!(inbox.status().pending, 0);
        assert!(inbox.adopting());
        drop(first);
        assert!(inbox.adopting());
        drop(second);
        assert!(!inbox.adopting());
        assert!(inbox.claim_for_adoption(2).is_none());
        assert!(!inbox.adopting());
    }

    #[test]
    fn a_failed_or_panicking_consumer_releases_its_adoption() {
        fn fail(inbox: &UserInbox) -> Result<(), ()> {
            let _adoption = inbox.claim_for_adoption(1).unwrap();
            assert!(inbox.adopting());
            Err(())
        }
        let inbox = UserInbox::default();
        enqueue(&inbox);
        assert_eq!(fail(&inbox), Err(()));
        assert!(!inbox.adopting());
        enqueue(&inbox);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _adoption = inbox.claim_for_adoption(2).unwrap();
                assert!(inbox.adopting());
                panic!("consumer stopped");
            }))
            .is_err()
        );
        assert!(!inbox.adopting());
    }

    #[tokio::test]
    async fn claim_gate_releases_all_locks_and_is_local_to_one_inbox() {
        let inbox = Arc::new(UserInbox::default());
        enqueue(&inbox);
        let other = UserInbox::default();
        enqueue(&other);
        let (reached, held) = tokio::sync::oneshot::channel();
        let (resume, resumed) = std::sync::mpsc::channel();
        inbox.gate_next_adoption(reached, resumed);
        let consumer = {
            let inbox = inbox.clone();
            std::thread::spawn(move || inbox.claim_for_adoption(1).unwrap())
        };
        held.await.unwrap();
        assert_eq!(inbox.status().pending, 0);
        assert!(inbox.adopting());
        drop(other.claim_for_adoption(1).unwrap());
        assert!(!other.adopting());
        // Observation/enqueue never waits on the test gate.
        enqueue(&inbox);
        assert_eq!(inbox.status().pending, 1);
        resume.send(()).unwrap();
        drop(consumer.join().unwrap());
        assert!(!inbox.adopting());
    }
}
