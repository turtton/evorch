//! Delivery observation for the user inbox, separate from command acceptance.
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

/// Text, images, and whether this input came from the host user (rather than an
/// internal team notification). Only host input contributes to the GUI queue.
/// The fourth field is its durable prompt UUID; the fifth is an incarnation-local
/// enqueue sequence (zero for internal notices), never a durable identity.
pub(crate) type UserInput = (String, Vec<crate::DelegateImage>, bool, Option<String>, u64);

/// Receiving/draining is not adoption. Keep a removable staging buffer so a
/// withdrawal releases its bounded channel slot even during an in-flight tool.
/// The receiver owns the channel; UserInbox holds only a weak reference, so
/// dropping the agent loop still closes the sender normally.
#[derive(Debug)]
pub(crate) struct InboxReceiver(Arc<Mutex<ReceiveState>>);

#[derive(Debug)]
struct ReceiveState {
    channel: tokio::sync::mpsc::Receiver<UserInput>,
    staged: VecDeque<UserInput>,
}

impl From<tokio::sync::mpsc::Receiver<UserInput>> for InboxReceiver {
    fn from(channel: tokio::sync::mpsc::Receiver<UserInput>) -> Self {
        Self(Arc::new(Mutex::new(ReceiveState {
            channel,
            staged: VecDeque::new(),
        })))
    }
}

impl InboxReceiver {
    pub(crate) fn new(channel: tokio::sync::mpsc::Receiver<UserInput>, inbox: &UserInbox) -> Self {
        let receiver = Self::from(channel);
        *inbox
            .receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::downgrade(&receiver.0);
        receiver
    }

    pub(crate) async fn recv(&mut self) -> Option<UserInput> {
        std::future::poll_fn(|cx| {
            let mut state = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(input) = state.staged.pop_front() {
                std::task::Poll::Ready(Some(input))
            } else {
                state.channel.poll_recv(cx)
            }
        })
        .await
    }

    pub(crate) fn try_recv(&mut self) -> Result<UserInput, tokio::sync::mpsc::error::TryRecvError> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match state.staged.pop_front() {
            Some(input) => Ok(input),
            None => state.channel.try_recv(),
        }
    }

    pub(crate) fn close(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .channel
            .close();
    }
}

/// The prompt ID whose delivery receipt this input carries: host input with an
/// ID. Internal notices, goal wakes and agent messages are never tracked.
pub(crate) fn tracked_prompt_id(input: &UserInput) -> Option<&str> {
    input.3.as_deref().filter(|_| input.2)
}

/// Why a queued input cannot be withdrawn from this incarnation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FollowUpWithdrawalError {
    #[error("message already consumed or withdrawn")]
    AlreadyConsumed,
    #[error("unknown queued message")]
    Unknown,
    #[error("inbox closed")]
    Closed,
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
    pending: BTreeSet<u64>,
    prompts: HashMap<String, u64>,
    requested_through: u64,
    handoff: bool,
}

pub(crate) struct UserInbox {
    state: Mutex<InboxState>,
    wake: tokio::sync::watch::Sender<bool>,
    // Separate from observation: host preparation can access the run map, whose
    // readers may inspect status. Never hold state while preparing a host send.
    // Lock order: trusted_send -> consumption -> state. Never wait for storage or a
    // channel permit while holding state.
    trusted_send: Mutex<()>,
    // Linearizes context adoption against durable withdrawal. Storage never
    // runs under state; observation and run-map readers remain nonblocking.
    consumption: Mutex<()>,
    receiver: Mutex<Weak<Mutex<ReceiveState>>>,
    /// Set once under `trusted_send`: this incarnation accepts no more host input.
    sealed: AtomicBool,
    #[cfg(test)]
    pause: Mutex<Option<test_pause::SendPause>>,
    #[cfg(test)]
    claim_gate_contended: Mutex<Option<std::sync::mpsc::Sender<()>>>,
}

impl Default for UserInbox {
    fn default() -> Self {
        Self {
            state: Mutex::new(InboxState::default()),
            wake: tokio::sync::watch::channel(false).0,
            trusted_send: Mutex::new(()),
            consumption: Mutex::new(()),
            receiver: Mutex::new(Weak::new()),
            sealed: AtomicBool::new(false),
            #[cfg(test)]
            pause: Mutex::new(None),
            #[cfg(test)]
            claim_gate_contended: Mutex::new(None),
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
    pub(crate) fn enqueue<E>(
        &self,
        prompt_id: Option<&str>,
        send: impl FnOnce(u64) -> Result<(), E>,
    ) -> Result<(), E> {
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
        state.pending.insert(seq);
        if let Some(id) = prompt_id {
            state.prompts.insert(id.into(), seq);
        }
        if state.handoff {
            state.requested_through = seq;
        }
        self.refresh(&mut state);
        Ok(())
    }

    /// Claim immediately before context adoption, not when taking a request cut
    /// or draining the mpsc channel. A withdrawn input can never be adopted.
    pub(crate) fn claim(&self, seq: u64) -> bool {
        if seq == 0 {
            return true; // internal notification / initial input
        }
        #[cfg(test)]
        if let Some(reached) = self.claim_gate_contended.lock().unwrap().take() {
            // Signal an actual contended gate attempt, not just thread startup.
            assert!(matches!(
                self.consumption.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
            reached.send(()).unwrap();
        }
        let _guard = self
            .consumption
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let claimed = state.pending.remove(&seq);
        self.refresh(&mut state);
        claimed
    }

    pub(crate) fn is_pending(&self, seq: u64) -> bool {
        seq == 0
            || self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pending
                .contains(&seq)
    }

    /// Serialize with sends/sealing and context adoption. If persistence fails,
    /// leave the input pending; callers must not remove its GUI row.
    pub(crate) fn withdraw(
        &self,
        prompt_id: &str,
        persist: impl FnOnce() -> Result<(), crate::RuntimeError>,
    ) -> Result<(), crate::RuntimeError> {
        self.serialize_trusted_send(|| {
            let _guard = self
                .consumption
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let seq = {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let seq = *state
                    .prompts
                    .get(prompt_id)
                    .ok_or(FollowUpWithdrawalError::Unknown)?;
                if !state.pending.contains(&seq) {
                    return Err(FollowUpWithdrawalError::AlreadyConsumed.into());
                }
                if self.sealed() {
                    return Err(FollowUpWithdrawalError::Closed.into());
                }
                seq
            };
            persist()?;
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.pending.remove(&seq);
            self.refresh(&mut state);
            drop(state);
            if let Some(receiver) = self
                .receiver
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .upgrade()
            {
                let mut receiver = receiver
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                while let Ok(input) = receiver.channel.try_recv() {
                    receiver.staged.push_back(input);
                }
                receiver.staged.retain(|input| input.4 != seq);
            }
            Ok(())
        })
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
        state.status.pending = state.pending.len();
        state.status.next_turn_requested = state
            .pending
            .first()
            .is_some_and(|seq| *seq <= state.requested_through);
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
        assert_eq!(inbox.enqueue(None, |_| Err("full")), Err("full"));
        assert_eq!(inbox.status(), FollowUpStatus::default());
        inbox.enqueue(None, |_| Ok::<_, ()>(())).unwrap();
        inbox.enqueue(None, |_| Ok::<_, ()>(())).unwrap();
        inbox.request_next_turn();
        inbox.claim(1);
        assert_eq!(inbox.status().pending, 1);
        assert!(inbox.status().next_turn_requested);
        inbox.claim(2);
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
            .enqueue(None, |seq| {
                accepted = seq;
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_eq!(accepted, 1);
        assert_eq!(inbox.seal_for_next_request(), (1, true));
        // The cut doesn't permanently seal host acceptance. A later input stays
        // pending, but is outside this next-request snapshot.
        inbox
            .enqueue(None, |seq| {
                assert_eq!(seq, 2);
                Ok::<_, ()>(())
            })
            .unwrap();
        inbox.claim(1);
        assert_eq!(inbox.status().pending, 1);
        assert!(!inbox.status().next_turn_requested);
        let next_batch = inbox.begin_wait();
        assert!(*next_batch.borrow());
        assert_eq!(inbox.seal_for_next_request(), (2, true));
        inbox.claim(1); // stale acknowledgement is idempotent
        assert!(inbox.status().next_turn_requested);
        inbox.claim(2);
        assert_eq!(inbox.status(), FollowUpStatus::default());
    }

    #[test]
    fn old_consumption_does_not_clear_a_new_reservation_and_failed_send_has_no_sequence() {
        let inbox = UserInbox::default();
        let wake = inbox.begin_wait();
        assert_eq!(inbox.enqueue(None, |_| Err("full")), Err("full"));
        assert!(!*wake.borrow());
        inbox
            .enqueue(None, |seq| {
                assert_eq!(seq, 1);
                Ok::<_, ()>(())
            })
            .unwrap();
        assert!(*wake.borrow());
        let cut = inbox.seal_for_next_request();
        assert_eq!(cut, (1, true));
        let next = inbox.begin_wait();
        inbox
            .enqueue(None, |seq| {
                assert_eq!(seq, 2);
                Ok::<_, ()>(())
            })
            .unwrap();
        inbox.claim(1);
        assert!(*next.borrow());
        assert_eq!(inbox.status().pending, 1);
        assert_eq!(inbox.seal_for_next_request(), (2, true));
        inbox.claim(1); // stale acknowledgement is idempotent
        assert!(inbox.status().next_turn_requested);
        inbox.claim(2);
        assert!(!*next.borrow());
        // An old incarnation's wake cannot affect a restored inbox even when
        // their first sequence values coincide.
        let restored = UserInbox::default();
        restored
            .enqueue(None, |seq| {
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
        inbox.enqueue(None, |_| Ok::<_, ()>(())).unwrap();
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

#[cfg(test)]
mod withdrawal_tests {
    use super::*;

    fn queued(inbox: &UserInbox, id: &str) -> u64 {
        let mut accepted = 0;
        inbox
            .enqueue(Some(id), |seq| {
                accepted = seq;
                Ok::<_, ()>(())
            })
            .unwrap();
        accepted
    }

    #[test]
    fn cut_does_not_consume_and_withdrawal_preserves_newer_reservations() {
        let inbox = UserInbox::default();
        let first = queued(&inbox, "first");
        let second = queued(&inbox, "second");
        inbox.request_next_turn();
        assert_eq!(inbox.seal_for_next_request(), (second, true));
        let newer = queued(&inbox, "newer");
        inbox.withdraw("first", || Ok(())).unwrap();
        assert_eq!(inbox.status().pending, 2);
        assert!(inbox.status().next_turn_requested);
        assert!(!inbox.claim(first));
        assert!(inbox.claim(second));
        assert_eq!(inbox.status().pending, 1);
        assert!(!inbox.status().next_turn_requested);
        inbox.request_next_turn();
        inbox.withdraw("newer", || Ok(())).unwrap();
        assert!(!inbox.claim(newer));
        assert_eq!(inbox.status(), FollowUpStatus::default());
        assert_eq!(
            inbox.withdraw("second", || Ok(())),
            Err(FollowUpWithdrawalError::AlreadyConsumed.into())
        );
        assert_eq!(
            inbox.withdraw("missing", || Ok(())),
            Err(FollowUpWithdrawalError::Unknown.into())
        );
    }

    #[test]
    fn failed_persistence_and_closed_inbox_leave_input_pending() {
        let inbox = UserInbox::default();
        let seq = queued(&inbox, "one");
        let error = crate::RuntimeError::FollowUpWithdrawal(FollowUpWithdrawalError::Unknown);
        assert_eq!(inbox.withdraw("one", || Err(error.clone())), Err(error));
        assert_eq!(inbox.status().pending, 1);
        inbox.seal();
        assert_eq!(
            inbox.withdraw("one", || panic!("must not persist")),
            Err(FollowUpWithdrawalError::Closed.into())
        );
        assert!(inbox.claim(seq));
    }

    #[test]
    fn adoption_cannot_overtake_a_committing_withdrawal() {
        let inbox = std::sync::Arc::new(UserInbox::default());
        let seq = queued(&inbox, "one");
        let (reached_tx, reached) = std::sync::mpsc::channel();
        let (resume, resume_rx) = std::sync::mpsc::channel();
        let withdrawal = {
            let inbox = inbox.clone();
            std::thread::spawn(move || {
                inbox.withdraw("one", || {
                    reached_tx.send(()).unwrap();
                    resume_rx.recv().unwrap();
                    Ok(())
                })
            })
        };
        reached.recv().unwrap();
        // Observation does not take the consumption/storage gate.
        assert_eq!(inbox.status().pending, 1);
        let (claim_reached_tx, claim_reached) = std::sync::mpsc::channel();
        *inbox.claim_gate_contended.lock().unwrap() = Some(claim_reached_tx);
        let adoption = {
            let inbox = inbox.clone();
            std::thread::spawn(move || inbox.claim(seq))
        };
        claim_reached.recv().unwrap();
        resume.send(()).unwrap();
        assert_eq!(withdrawal.join().unwrap(), Ok(()));
        assert!(!adoption.join().unwrap());
        assert_eq!(inbox.status().pending, 0);
    }
}

#[cfg(test)]
mod channel_withdrawal_tests {
    use super::*;

    #[tokio::test]
    async fn full_queue_withdrawal_reclaims_capacity_and_keeps_fifo_until_receiver_drop() {
        let inbox = UserInbox::default();
        let (sender, channel) = tokio::sync::mpsc::channel(2);
        let mut receiver = InboxReceiver::new(channel, &inbox);
        for id in ["first", "withdraw"] {
            inbox
                .enqueue(Some(id), |seq| {
                    sender.try_send((id.into(), vec![], true, Some(id.into()), seq))
                })
                .unwrap();
        }
        assert_eq!(sender.capacity(), 0);
        inbox.request_next_turn();
        let (cut, _) = inbox.seal_for_next_request();
        inbox.withdraw("withdraw", || Ok(())).unwrap();
        inbox
            .enqueue(Some("new"), |seq| {
                sender.try_send(("new".into(), vec![], true, Some("new".into()), seq))
            })
            .unwrap();
        let first = receiver.recv().await.unwrap();
        let newer = receiver.recv().await.unwrap();
        assert_eq!((first.0.as_str(), newer.0.as_str()), ("first", "new"));
        assert!(first.4 <= cut && newer.4 > cut);
        assert!(inbox.claim(first.4));
        assert!(inbox.claim(newer.4));
        assert_eq!(inbox.status().pending, 0);
        assert!(receiver.try_recv().is_err());
        drop(receiver);
        assert!(sender.is_closed());
    }
}
