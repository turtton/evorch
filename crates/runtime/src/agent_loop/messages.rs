use event_bus::{AgentMessage, AgentMessageKind};

use super::LoopState;

const AGENT_MESSAGE_PREFIX: &str = "agent-message";

impl LoopState {
    pub(crate) fn begin_internal_input(&mut self) {
        self.turn = crate::request_turn::internal(self.task.ancestry);
    }

    pub(crate) fn accept_input_identity(&mut self, trusted_user: bool, prompt_id: Option<String>) {
        if trusted_user && let Some(prompt_id) = prompt_id {
            self.turn =
                crate::request_turn::human(prompt_id, self.task.ancestry, false, Some(&self.turn));
        } else {
            self.begin_internal_input();
        }
    }

    pub(crate) fn has_pending_user_messages(&self) -> bool {
        self.pending_user_messages
            .iter()
            .any(|message| self.channels.user_inbox.is_pending(message.4))
    }

    pub(crate) fn queue_user_message(&mut self, message: crate::runtime::user_inbox::UserInput) {
        if self.channels.user_inbox.is_pending(message.4) {
            self.pending_user_messages.push(message);
        }
    }

    pub(super) fn flush_user_messages(&mut self) -> bool {
        let (cut, _) = self.channels.user_inbox.seal_for_next_request();
        self.flush_user_messages_through(cut)
    }

    pub(super) fn flush_scheduled_user_messages(&mut self) -> bool {
        let (cut, requested) = self.channels.user_inbox.seal_for_next_request();
        if self.has_pending_user_messages() || requested {
            return self.flush_user_messages_through(cut);
        }
        false
    }

    fn flush_user_messages_through(&mut self, cut: u64) -> bool {
        let mut messages = std::mem::take(&mut self.pending_user_messages);
        while let Ok(message) = self.channels.inbox_rx.try_recv() {
            messages.push(message);
        }
        let mut later = Vec::new();
        messages.retain(|message| {
            if message.2 && message.4 > cut {
                later.push(message.clone());
                false
            } else {
                true
            }
        });
        self.pending_user_messages = later;
        // A trusted goal wake is a scheduling signal, not a new human request.
        // When already running, the normal Stop boundary will perform the check.
        messages.retain(|(text, _, trusted_user, _, _)| {
            if !trusted_user && text == crate::thread_goals::CHECKS_WAKE {
                self.goal_wake_pending = true;
                false
            } else {
                true
            }
        });
        let mut received = false;
        for message in messages {
            received |= self.take_user_input(message);
        }
        if received {
            self.publish_message_count();
            self.resumed = true;
        }
        received
    }

    pub(super) fn flush_aside(&mut self) -> bool {
        let received = self.flush_user_messages();
        if self.task.mailbox.is_empty() {
            return received;
        }
        let messages = self.task.mailbox.drain_where(|_| true);
        if messages.is_empty() {
            return received;
        }
        self.inject_messages(messages);
        // Aside は turn 境界で新しい入力となり、次の clean Stop で完了する。
        self.resumed = true;
        true
    }

    pub(super) fn inject_parent_messages(&mut self) {
        let Some(parent) = self.task.parent else {
            return;
        };
        let parent = parent.to_string();
        let messages = self
            .task
            .mailbox
            .drain_where(|message| message.sender_run_id == parent);
        self.inject_messages(messages);
    }

    pub(super) fn inject_messages(&mut self, messages: Vec<AgentMessage>) {
        if messages.is_empty() {
            return;
        }
        self.begin_internal_input();
        for message in messages {
            self.context.push_user(&format_agent_message(&message));
        }
        self.publish_message_count();
    }
}

pub(super) fn format_agent_message(message: &AgentMessage) -> String {
    let kind = match message.kind {
        AgentMessageKind::Send => "send",
        AgentMessageKind::Reply => "reply",
        AgentMessageKind::Steering => "steering",
    };
    format!(
        "[{AGENT_MESSAGE_PREFIX} id={} from={} kind={kind}]\n{}",
        message.message_id, message.sender_run_id, message.content
    )
}

#[cfg(test)]
mod shell_handoff_tests {
    use std::sync::Arc;
    use tools::executor::ShellInputControl;

    #[tokio::test]
    async fn next_request_cut_appends_inputs_once_and_keeps_newer_reservation() {
        let (_cancel, cancelled) = tokio::sync::watch::channel(crate::RunInterrupt::None);
        let (_runtime, mut state) = super::super::delegate_cleanup_tests::fixture(cancelled);
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        state.channels.inbox_rx = receiver.into();
        let inbox = Arc::new(crate::runtime::user_inbox::UserInbox::default());
        state.channels.user_inbox = inbox.clone();
        let first = inbox.begin_wait();
        let parallel = inbox.begin_wait();
        inbox
            .enqueue(None, |seq| {
                sender.try_send(("before cut".into(), vec![], true, Some("one".into()), seq))
            })
            .unwrap();
        assert!(*first.borrow() && *parallel.borrow());
        drop(first);
        drop(parallel); // Reservation survives receiver teardown.
        let (cut, requested) = inbox.seal_for_next_request();
        assert!(requested);
        let _new_wait = inbox.begin_wait();
        inbox
            .enqueue(None, |seq| {
                sender.try_send(("after cut".into(), vec![], true, Some("two".into()), seq))
            })
            .unwrap();
        assert!(state.flush_user_messages_through(cut));
        assert_eq!(inbox.status().pending, 1);
        assert!(inbox.status().next_turn_requested);
        assert_eq!(state.pending_user_messages.len(), 1);
        assert!(state.host_inputs().contains("one"));
        assert!(!state.host_inputs().contains("two"));
        state.flush_scheduled_user_messages();
        assert_eq!(inbox.status().pending, 0);
        assert!(!inbox.status().next_turn_requested);
        assert!(state.host_inputs().contains("two"));
        assert!(!state.flush_user_messages());
        let history = serde_json::to_string(&state.context.messages).unwrap();
        assert_eq!(history.matches("before cut").count(), 1);
        assert_eq!(history.matches("after cut").count(), 1);
    }

    #[tokio::test]
    async fn withdrawal_after_cut_and_after_wait_drain_never_enters_context() {
        let (_cancel, cancelled) = tokio::sync::watch::channel(crate::RunInterrupt::None);
        let (_runtime, mut state) = super::super::delegate_cleanup_tests::fixture(cancelled);
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        state.channels.inbox_rx = receiver.into();
        let inbox = state.channels.user_inbox.clone();
        for id in ["from-wait", "from-channel", "keep"] {
            inbox
                .enqueue(Some(id), |seq| {
                    sender.try_send((id.into(), vec![], true, Some(id.into()), seq))
                })
                .unwrap();
        }
        // Simulate meta/runs/waiting having drained one input into LoopState;
        // the other remains in mpsc. Neither has entered the context.
        let input = state.channels.inbox_rx.recv().await.unwrap();
        state.queue_user_message(input);
        inbox.request_next_turn();
        let (cut, requested) = inbox.seal_for_next_request();
        assert!(requested);
        inbox.withdraw("from-channel", || Ok(())).unwrap();
        inbox.withdraw("from-wait", || Ok(())).unwrap();
        assert!(!state.has_pending_user_messages());
        assert!(state.flush_user_messages_through(cut));
        assert!(state.host_inputs().contains("keep"));
        assert!(!state.host_inputs().contains("from-channel"));
        assert!(!state.host_inputs().contains("from-wait"));
        let context = serde_json::to_string(&state.context.messages).unwrap();
        assert!(!context.contains("from-channel") && !context.contains("from-wait"));
        assert_eq!(inbox.status().pending, 0);
        assert_eq!(
            inbox.withdraw("keep", || panic!("already adopted")),
            Err(crate::FollowUpWithdrawalError::AlreadyConsumed.into())
        );
    }
}

#[cfg(test)]
mod waiting_cut_tests {
    use super::super::delegate_cleanup_tests;
    use crate::runtime::user_inbox::SendPausePoint;

    #[tokio::test]
    async fn wait_ignores_withdrawn_input_until_a_real_terminal_event() {
        let (_cancel, cancelled) = tokio::sync::watch::channel(crate::RunInterrupt::None);
        let (runtime, mut state) = delegate_cleanup_tests::started_fixture(cancelled).await;
        let parent = state.caller_run_id();
        let child = runtime
            .delegate_background_as_child(
                parent,
                crate::Role::Worker,
                "child",
                crate::RunConfig::default(),
            )
            .unwrap();
        state
            .transition(event_bus::AgentRunPhase::Running, None)
            .unwrap();
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        state.channels.inbox_rx = receiver.into();
        let inbox = state.channels.user_inbox.clone();
        inbox
            .enqueue(Some("withdraw"), |seq| {
                sender.try_send((
                    "never inject".into(),
                    vec![],
                    true,
                    Some("withdraw".into()),
                    seq,
                ))
            })
            .unwrap();
        inbox.withdraw("withdraw", || Ok(())).unwrap();
        {
            let wait = crate::meta::dispatch(
                &mut state,
                "wait",
                serde_json::json!({"run_ids":[child.to_string()]}),
            );
            tokio::pin!(wait);
            assert!(futures_util::poll!(&mut wait).is_pending());
            runtime.cancel(child).unwrap();
            runtime.wait(child).await.unwrap();
            let result = wait.await.result;
            assert!(!result.is_error, "{}", result.content);
            let result: serde_json::Value = serde_json::from_str(&result.content).unwrap();
            assert_eq!(result["user_input_ready"], false);
        }
        assert!(!state.has_pending_user_messages());
        runtime.cancel(parent).unwrap();
        runtime.wait(parent).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enqueue_between_cut_and_drain_resumes_local_pending_without_another_event() {
        let (_cancel, cancelled) = tokio::sync::watch::channel(crate::RunInterrupt::None);
        let (runtime, mut state) = delegate_cleanup_tests::started_fixture(cancelled).await;
        state
            .transition(event_bus::AgentRunPhase::Running, None)
            .unwrap();
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        state.channels.inbox_rx = receiver.into();
        let inbox = state.channels.user_inbox.clone();
        let pause = inbox.pause_next_send(SendPausePoint::AfterRequestCut);
        let enqueuer = std::thread::spawn(move || {
            pause.reached.recv().unwrap();
            inbox
                .enqueue(None, |seq| {
                    sender.try_send((
                        "after last cut".into(),
                        vec![],
                        true,
                        Some("late".into()),
                        seq,
                    ))
                })
                .unwrap();
            pause.resume.send(()).unwrap();
            sender // Keep the channel open without sending another event.
        });
        assert!(!state.flush_aside());
        let _sender = enqueuer.join().unwrap();
        assert_eq!(state.pending_user_messages.len(), 1);
        assert!(state.wait_for_input().await);
        assert!(state.pending_user_messages.is_empty());
        assert_eq!(state.channels.user_inbox.status().pending, 0);
        assert!(state.host_inputs().contains("late"));
        let next_request = serde_json::to_string(&state.context.messages).unwrap();
        assert_eq!(next_request.matches("after last cut").count(), 1);
        runtime.cancel(state.caller_run_id()).unwrap();
        runtime.wait(state.caller_run_id()).await.unwrap();
    }
}
