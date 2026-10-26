//! Delivery observation for the user inbox, separate from command acceptance.
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Text, images, and whether this input came from the host user (rather than an
/// internal team notification). Only host input contributes to the GUI queue.
/// The final field is the UUID captured when host input was accepted.
pub(crate) type UserInput = (String, Vec<crate::DelegateImage>, bool, Option<String>);

/// The prompt ID whose delivery receipt this input carries: host input with an
/// ID. Internal notices, goal wakes and agent messages are never tracked.
pub(crate) fn tracked_prompt_id(input: &UserInput) -> Option<&str> {
    input.3.as_deref().filter(|_| input.2)
}

/// Pending input has not yet been appended to the agent's context.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FollowUpStatus {
    pub pending: usize,
    pub next_turn_requested: bool,
    pub closed: bool,
}

#[derive(Default)]
pub(crate) struct UserInbox {
    state: Mutex<FollowUpStatus>,
    // Separate from observation: host preparation can access the run map, whose
    // readers may inspect status. Never hold state while preparing a host send.
    // Lock order: trusted_send before state. Never wait for storage or a
    // channel permit while holding state.
    trusted_send: Mutex<()>,
    /// Set once under `trusted_send`: this incarnation accepts no more host input.
    sealed: AtomicBool,
    #[cfg(test)]
    pause: Mutex<Option<test_pause::SendPause>>,
}

impl UserInbox {
    pub(crate) fn serialize_trusted_send<E>(
        &self,
        send: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), E> {
        let _guard = self
            .trusted_send
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        send()
    }

    /// Close this incarnation to host input for good. Waits for a host send in
    /// progress, so afterwards every accepted input is in the channel or already
    /// consumed, and later sends are refused before leaving any trace.
    /// Blocks: async callers use [`seal_off_thread`].
    pub(crate) fn seal(&self) {
        let _guard = self
            .trusted_send
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.sealed.store(true, Ordering::Release);
    }

    /// Checked by host sends while they hold the trusted-send lock.
    pub(crate) fn sealed(&self) -> bool {
        self.sealed.load(Ordering::Acquire)
    }

    // Hold the observation lock through enqueueing so consumption cannot race
    // ahead of registration. A rejected send must not increase the count.
    pub(crate) fn enqueue<E>(&self, send: impl FnOnce() -> Result<(), E>) -> Result<(), E> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        send()?;
        state.pending += 1;
        Ok(())
    }

    pub(crate) fn consumed(&self, count: usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending = state.pending.saturating_sub(count);
        if state.pending == 0 {
            state.next_turn_requested = false;
        }
    }

    pub(crate) fn request_next_turn(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.next_turn_requested = state.pending > 0;
    }

    pub(crate) fn status(&self) -> FollowUpStatus {
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A test stop in a host send; no-op outside tests.
    pub(crate) fn pause_point(&self, _point: SendPausePoint) {
        #[cfg(test)]
        {
            let pause = self
                .pause
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take_if(|pause| pause.point == _point);
            if let Some(pause) = pause {
                pause.wait();
            }
        }
    }
}

/// Where a test may stop a host send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SendPausePoint {
    /// After the phase check, before taking the trusted-send lock.
    BeforeLock,
    /// Holding the lock with a reserved channel permit, before persistence.
    AfterReserve,
}

/// Seal without blocking an async worker: the seal may wait for a host send
/// that is writing to storage.
pub(crate) async fn seal_off_thread(inbox: std::sync::Arc<UserInbox>) {
    match tokio::task::spawn_blocking(move || inbox.seal()).await {
        Ok(()) => {}
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => tracing::warn!(%error, "inbox seal did not finish"),
    }
}

#[cfg(test)]
pub(crate) mod test_pause {
    use std::sync::mpsc;

    pub(crate) struct SendPause {
        pub(super) point: super::SendPausePoint,
        reached: mpsc::Sender<()>,
        resume: mpsc::Receiver<()>,
    }

    impl SendPause {
        pub(super) fn wait(self) {
            let _ = self.reached.send(());
            let _ = self.resume.recv();
        }
    }

    /// Control of one armed stop: wait until a send reaches it, then let it go.
    pub(crate) struct PauseControl {
        pub(crate) reached: mpsc::Receiver<()>,
        pub(crate) resume: mpsc::Sender<()>,
    }

    impl super::UserInbox {
        /// Stop the next host send that reaches `point` until resumed.
        pub(crate) fn pause_next_send(&self, point: super::SendPausePoint) -> PauseControl {
            let (reached_tx, reached) = mpsc::channel();
            let (resume, resume_rx) = mpsc::channel();
            *self
                .pause
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(SendPause {
                point,
                reached: reached_tx,
                resume: resume_rx,
            });
            PauseControl { reached, resume }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_sends_and_consumed_batches_do_not_leave_phantom_pending_input() {
        let inbox = UserInbox::default();
        assert_eq!(inbox.enqueue(|| Err("full")), Err("full"));
        assert_eq!(inbox.status(), FollowUpStatus::default());
        inbox.enqueue(|| Ok::<_, ()>(())).unwrap();
        inbox.enqueue(|| Ok::<_, ()>(())).unwrap();
        inbox.request_next_turn();
        inbox.consumed(1);
        assert_eq!(inbox.status().pending, 1);
        assert!(inbox.status().next_turn_requested);
        inbox.consumed(1);
        assert_eq!(inbox.status(), FollowUpStatus::default());
        inbox.request_next_turn();
        assert_eq!(inbox.status(), FollowUpStatus::default());
    }
}
