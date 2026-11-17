use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// Ownership of consumed input until its execution phase has been published.
/// It owns no inbox lock and also releases on an error, cancellation or unwind.
/// This is execution-local state, never part of the persisted inbox/receipt.
pub(crate) struct InputAdoption(Arc<AtomicUsize>);

impl Drop for InputAdoption {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl super::UserInbox {
    pub(super) fn begin_adoption(&self) -> InputAdoption {
        self.adoptions.fetch_add(1, Ordering::AcqRel);
        InputAdoption(self.adoptions.clone())
    }

    pub(crate) fn adopting(&self) -> bool {
        self.adoptions.load(Ordering::Acquire) != 0
    }
}

#[cfg(any(test, feature = "idle-test-support"))]
pub(super) struct Gate {
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
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Gate { reached, resume });
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
}

#[cfg(test)]
mod tests {
    use super::super::UserInbox;
    use std::sync::Arc;

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
