//! Delivery observation for the user inbox, separate from command acceptance.
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Text, images, and whether this input came from the host user (rather than an
/// internal team notification). Only host input contributes to the GUI queue.
/// The fourth field is its durable prompt UUID; the fifth is an incarnation-local
/// enqueue sequence (zero for internal notices), never a durable identity.
pub(crate) type UserInput = (String, Vec<crate::DelegateImage>, bool, Option<String>, u64);

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

#[derive(Debug, Default)]
struct InboxState {
    status: FollowUpStatus,
    accepted: u64,
    consumed: u64,
    requested_through: u64,
    handoff: bool,
}

pub(crate) struct UserInbox {
    state: Mutex<InboxState>,
    wake: tokio::sync::watch::Sender<bool>,
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

impl Default for UserInbox {
    fn default() -> Self {
        Self {
            state: Mutex::new(InboxState::default()),
            wake: tokio::sync::watch::channel(false).0,
            trusted_send: Mutex::new(()),
            sealed: AtomicBool::new(false),
            #[cfg(test)]
            pause: Mutex::new(None),
        }
    }
}

impl std::fmt::Debug for UserInbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserInbox")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl tools::executor::ShellInputControl for UserInbox {
    fn begin_wait(&self) -> tokio::sync::watch::Receiver<bool> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.handoff = true;
        state.requested_through = state.accepted;
        self.refresh(&mut state);
        self.wake.subscribe()
    }
}

/// What ends a run's blocking shell poll: host input, or a message from its
/// parent. Both reach the next model request at the post-tools boundary
/// (host input through the handoff cut, parent messages by injection), so
/// waking for them never leaves input unread.
#[derive(Debug)]
pub(crate) struct RunShellInput {
    pub(crate) inbox: std::sync::Arc<UserInbox>,
    pub(crate) mailbox: std::sync::Arc<crate::mailbox::RunMailbox>,
    pub(crate) parent: Option<String>,
}

impl tools::executor::ShellInputControl for RunShellInput {
    fn begin_wait(&self) -> tokio::sync::watch::Receiver<bool> {
        self.inbox.begin_wait()
    }

    fn wait_for_input(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let host = tools::executor::ShellInputControl::wait_for_input(&*self.inbox);
        let mailbox = std::sync::Arc::clone(&self.mailbox);
        let parent = self.parent.clone();
        Box::pin(async move {
            let from_parent = async move {
                let Some(parent) = parent else {
                    return std::future::pending::<()>().await;
                };
                // Subscribe before checking so a delivery in between still wakes.
                let mut version = mailbox.subscribe_version();
                while !mailbox.has_interrupting_message_from(&parent) {
                    if version.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                }
            };
            tokio::select! {
                () = host => {}
                () = from_parent => {}
            }
        })
    }
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
    pub(crate) fn enqueue<E>(&self, send: impl FnOnce(u64) -> Result<(), E>) -> Result<(), E> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let seq = state
            .accepted
            .checked_add(1)
            .expect("inbox sequence exhausted");
        send(seq)?;
        state.accepted = seq;
        state.status.pending += 1;
        if state.handoff {
            state.requested_through = seq;
        }
        self.refresh(&mut state);
        Ok(())
    }

    /// Acknowledge the ordered prefix actually appended, not a notification or
    /// durable Delivered receipt. A repeated/old cut cannot consume newer input.
    pub(crate) fn consumed_through(&self, seq: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let through = seq.min(state.accepted).max(state.consumed);
        let count = through - state.consumed;
        state.consumed = through;
        state.status.pending = state.status.pending.saturating_sub(count as usize);
        self.refresh(&mut state);
    }

    pub(crate) fn request_next_turn(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.requested_through = state.accepted;
        self.refresh(&mut state);
    }

    pub(crate) fn status(&self) -> FollowUpStatus {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status
    }

    fn refresh(&self, state: &mut InboxState) {
        state.status.next_turn_requested = state.requested_through > state.consumed;
        self.wake.send_replace(state.status.next_turn_requested);
    }

    /// Fix the next request's host-input cut. Unlike seal(), acceptance stays
    /// open. The lock linearizes this cut against successful enqueue and keeps
    /// the reservation alive after every shell receiver has gone away.
    pub(crate) fn seal_for_next_request(&self) -> (u64, bool) {
        let cut = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.handoff = false;
            (state.accepted, state.status.next_turn_requested)
        };
        self.pause_point(SendPausePoint::AfterRequestCut);
        cut
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
    /// The request cut is fixed, before draining the channel. No state lock held.
    AfterRequestCut,
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
        assert_eq!(inbox.enqueue(|_| Err("full")), Err("full"));
        assert_eq!(inbox.status(), FollowUpStatus::default());
        inbox.enqueue(|_| Ok::<_, ()>(())).unwrap();
        inbox.enqueue(|_| Ok::<_, ()>(())).unwrap();
        inbox.request_next_turn();
        inbox.consumed_through(1);
        assert_eq!(inbox.status().pending, 1);
        assert!(inbox.status().next_turn_requested);
        inbox.consumed_through(2);
        assert_eq!(inbox.status(), FollowUpStatus::default());
        inbox.request_next_turn();
        assert_eq!(inbox.status(), FollowUpStatus::default());
    }
}

#[cfg(test)]
mod handoff_tests {
    use super::*;
    use tools::executor::ShellInputControl;

    #[test]
    fn enqueue_is_reserved_until_cut_even_after_last_shell_receiver_is_gone() {
        let inbox = UserInbox::default();
        let first = inbox.begin_wait();
        let second = inbox.begin_wait();
        assert!(!*first.borrow());
        drop(first);
        drop(second);
        let mut accepted = 0;
        inbox
            .enqueue(|seq| {
                accepted = seq;
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_eq!(accepted, 1);
        assert_eq!(inbox.seal_for_next_request(), (1, true));
        // The cut doesn't permanently seal host acceptance. A later input stays
        // pending, but is outside this next-request snapshot.
        inbox
            .enqueue(|seq| {
                assert_eq!(seq, 2);
                Ok::<_, ()>(())
            })
            .unwrap();
        inbox.consumed_through(1);
        assert_eq!(inbox.status().pending, 1);
        assert!(!inbox.status().next_turn_requested);
        let next_batch = inbox.begin_wait();
        assert!(*next_batch.borrow());
        assert_eq!(inbox.seal_for_next_request(), (2, true));
        inbox.consumed_through(1); // stale acknowledgement is idempotent
        assert!(inbox.status().next_turn_requested);
        inbox.consumed_through(2);
        assert_eq!(inbox.status(), FollowUpStatus::default());
    }

    #[test]
    fn old_consumption_does_not_clear_a_new_reservation_and_failed_send_has_no_sequence() {
        let inbox = UserInbox::default();
        let wake = inbox.begin_wait();
        assert_eq!(inbox.enqueue(|_| Err("full")), Err("full"));
        assert!(!*wake.borrow());
        inbox
            .enqueue(|seq| {
                assert_eq!(seq, 1);
                Ok::<_, ()>(())
            })
            .unwrap();
        assert!(*wake.borrow());
        let cut = inbox.seal_for_next_request();
        assert_eq!(cut, (1, true));
        let next = inbox.begin_wait();
        inbox
            .enqueue(|seq| {
                assert_eq!(seq, 2);
                Ok::<_, ()>(())
            })
            .unwrap();
        inbox.consumed_through(1);
        assert!(*next.borrow());
        assert_eq!(inbox.status().pending, 1);
        assert_eq!(inbox.seal_for_next_request(), (2, true));
        inbox.consumed_through(1); // stale acknowledgement is idempotent
        assert!(inbox.status().next_turn_requested);
        inbox.consumed_through(2);
        assert!(!*next.borrow());
        // An old incarnation's wake cannot affect a restored inbox even when
        // their first sequence values coincide.
        let restored = UserInbox::default();
        restored
            .enqueue(|seq| {
                assert_eq!(seq, 1);
                Ok::<_, ()>(())
            })
            .unwrap();
        assert!(!restored.status().next_turn_requested);
        inbox.request_next_turn();
        assert!(!restored.status().next_turn_requested);
    }

    #[test]
    fn send_sooner_is_a_reservation_but_cut_does_not_replace_permanent_seal() {
        let inbox = UserInbox::default();
        inbox.enqueue(|_| Ok::<_, ()>(())).unwrap();
        assert!(!inbox.status().next_turn_requested);
        inbox.request_next_turn();
        assert_eq!(inbox.seal_for_next_request(), (1, true));
        assert!(!inbox.sealed());
        inbox.seal();
        assert!(inbox.sealed());
        assert_eq!(inbox.seal_for_next_request(), (1, true));
        assert!(inbox.sealed());
    }
}
