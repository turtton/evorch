//! Delivery observation for the user inbox, separate from command acceptance.
use std::sync::Mutex;

/// Text, images, and whether this input came from the host user (rather than an
/// internal team notification). Only host input contributes to the GUI queue.
/// The final field is the UUID captured when host input was accepted.
pub(crate) type UserInput = (String, Vec<crate::DelegateImage>, bool, Option<String>);

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
    trusted_send: Mutex<()>,
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
