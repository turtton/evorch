use super::LoopState;
use crate::runtime::user_inbox::UserInput;
use tokio::sync::watch;

type Changed = Result<(), watch::error::RecvError>;

pub(super) enum InputWake {
    Quiesce,
    Cancel(Changed),
    Deliveries(Changed),
    Answers(Changed),
    Inbox(Option<UserInput>),
    Mailbox(Changed),
}

impl LoopState {
    pub(super) async fn next_input_wake(
        &mut self,
        quiesce: &mut watch::Receiver<u64>,
        deliveries: &mut watch::Receiver<u64>,
        answers: &mut watch::Receiver<u64>,
    ) -> InputWake {
        // A wake that did not resume execution has finished handling its input.
        // Resuming and terminal paths instead release after publication or drop.
        self.wake_adoption = None;
        let inbox = self.channels.user_inbox.clone();
        let (wake, adoption) = inbox
            .adopt_wake(async {
                tokio::select! {
                    biased;
                    _ = quiesce.changed() => InputWake::Quiesce,
                    changed = self.channels.cancel_rx.changed() => InputWake::Cancel(changed),
                    changed = deliveries.changed() => InputWake::Deliveries(changed),
                    changed = answers.changed() => InputWake::Answers(changed),
                    message = self.channels.inbox_rx.recv() => InputWake::Inbox(message),
                    changed = self.channels.mailbox_version_rx.changed() => InputWake::Mailbox(changed),
                }
            })
            .await;
        self.wake_adoption = Some(adoption);
        wake
    }
}

#[cfg(test)]
mod tests;
