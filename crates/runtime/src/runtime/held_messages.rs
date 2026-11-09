//! Messages for a finished or stopped run wait until it is registered again.
//! A subagent's report wakes its parent unless the operator stopped it.
//! Automatic wakes without user input are bounded per conversation so that
//! agents cannot keep each other running indefinitely.

use super::*;

/// Automatic wakes allowed in one conversation between user inputs.
pub(crate) const AUTO_WAKE_LIMIT: u32 = 20;

impl AgentRuntime {
    /// How many messages wait for a recipient that is not running.
    #[must_use]
    pub fn held_message_count(&self) -> usize {
        self.shared
            .held_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(Vec::len)
            .sum()
    }

    /// How many automatic wakes have been spent, over every conversation.
    #[must_use]
    pub fn auto_wake_total(&self) -> u32 {
        self.shared
            .auto_wakes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .sum()
    }

    /// Hold `message` for a recipient that is not running. The caller holds the
    /// runs lock, so registration cannot release the queue concurrently.
    #[cfg(test)]
    pub(super) fn hold_message(&self, recipient: RunId, message: AgentMessage) {
        self.hold_queued_message(recipient, crate::mailbox::QueuedMessage::explicit(message));
    }

    pub(super) fn hold_queued_message(
        &self,
        recipient: RunId,
        queued: crate::mailbox::QueuedMessage,
    ) {
        if queued.observed() {
            return;
        }
        let message = queued.message.clone();
        self.shared
            .held_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(recipient)
            .or_default()
            .push(queued);
        self.shared
            .bus
            .emit(Event::new(AgentMessageEvent::Delivered {
                message,
                disposition: DeliveryDisposition::Held,
            }));
    }

    /// Move held messages into the mailbox of a run being registered. The
    /// caller holds the runs lock and subscribed to the mailbox version first,
    /// so a waiting run observes them.
    pub(super) fn release_held_messages(&self, run_id: RunId, mailbox: &RunMailbox) {
        self.shared
            .question_delivery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .root_requests
            .remove(&run_id);
        let held = self
            .shared
            .held_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&run_id)
            .unwrap_or_default();
        let mut sent = self
            .shared
            .sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut retry = Vec::new();
        for queued in held.into_iter().filter(|message| !message.observed()) {
            if queued
                .question_notice()
                .is_some_and(|n| n.paused.load(Ordering::Acquire))
            {
                retry.push(queued);
                continue;
            }
            let message = queued.message.clone();
            let sender = message.sender_run_id.parse::<RunId>().ok();
            match mailbox.try_push_queued(queued.clone()) {
                Ok(()) => {
                    if let Some(sender) = sender {
                        sent.insert(
                            message.message_id.clone(),
                            SentRecord {
                                sender,
                                recipient: run_id,
                            },
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(%run_id, ?error, "held message could not be released");
                    if queued.question_notice().is_some() {
                        retry.push(queued);
                    }
                }
            }
        }
        if !retry.is_empty() {
            self.shared
                .held_messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(run_id)
                .or_default()
                .splice(0..0, retry);
        }
    }

    /// The user spoke to this conversation: automatic wakes may continue.
    pub(crate) fn reset_auto_wakes(&self, root: RunId) {
        self.reset_question_wakes(root);
        self.schedule_question_recovery(root);
        self.shared
            .auto_wakes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&root);
    }

    /// Wake `run` after its locks are released, unless the operator stopped it.
    pub(super) fn schedule_wake(&self, run: RunId) {
        if self.operator_stopped(run) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::debug!(%run, "no async runtime to wake a held recipient");
            return;
        };
        let runtime = self.clone();
        handle.spawn(async move { runtime.wake(run).await });
    }

    pub(in crate::runtime) async fn wake(&self, run: RunId) {
        // A parent still finishing closed its mailbox; wake it once it settles.
        loop {
            let finishing = {
                let runs = lock_runs(&self.shared.runs);
                let Some(entry) = runs.get(&run) else {
                    return;
                };
                if !matches!(
                    *entry.phase_rx.borrow(),
                    AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
                ) {
                    break;
                }
                // Registration released the queue if the run came back meanwhile.
                if !entry.mailbox.is_closed() {
                    return;
                }
                entry.phase_rx.clone()
            };
            let mut phase = finishing;
            if phase
                .wait_for(|phase| {
                    !matches!(
                        phase,
                        AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
                    )
                })
                .await
                .is_err()
            {
                return;
            }
        }
        if self.operator_stopped(run) {
            return;
        }
        // Observation also takes runs before held_messages. Keep the root wake
        // decision under that lock, so acknowledging the last report cannot race
        // the empty-queue check and emit a spurious host continuation request.
        let runs = lock_runs(&self.shared.runs);
        let Some(entry) = runs.get(&run) else { return };
        if self
            .shared
            .question_delivery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .root_requests
            .contains(&run)
        {
            return;
        }
        let (ordinary, notices) = {
            let mut held = self
                .shared
                .held_messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(queue) = held.get_mut(&run) else {
                return;
            };
            queue.retain(|message| !message.observed());
            (
                queue.iter().any(|q| q.question_notice().is_none()),
                queue
                    .iter()
                    .filter_map(|q| q.question_notice().cloned())
                    .collect::<Vec<_>>(),
            )
        };
        if !ordinary && notices.is_empty() {
            return;
        }
        let parent = entry.parent;
        // Do not spend a guard attempting an unauthorized recursive ancestor wake.
        if parent.is_some_and(|p| runs.get(&p).is_none_or(|e| !is_live(e))) {
            return;
        }
        // Stop may have latched between the fast check and acquiring runs.
        if self.operator_stopped(run) {
            return;
        }
        let root = questions::delivery::conversation_root(&runs, run);
        let auto_available = self
            .shared
            .auto_wakes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&root)
            .is_none_or(|count| *count < AUTO_WAKE_LIMIT);
        if ordinary && auto_available && self.take_auto_wake(root) {
            for notice in &notices {
                notice.paused.store(false, Ordering::Release);
            }
        } else if !self.admit_question_batch(root, run, &notices, true) {
            // Ordinary-only exhausted batches keep the existing one-shot pause.
            // A question fallback does not debit (even the pause sentinel of) AUTO_WAKE.
            if ordinary {
                self.take_auto_wake(root);
            }
            return;
        }
        if parent.is_none() {
            self.shared
                .question_delivery
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .root_requests
                .insert(run);
        }
        match parent {
            None => {
                self.shared
                    .bus
                    .emit(Event::new(LifecycleEvent::RootWakeRequested {
                        run_id: run.to_string(),
                    }));
            }
            Some(parent) => {
                drop(runs);
                self.wake_subagent(parent, run);
            }
        }
    }

    /// Count one automatic wake, or announce the pause once the limit is reached.
    fn take_auto_wake(&self, root: RunId) -> bool {
        let mut wakes = self
            .shared
            .auto_wakes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = wakes.entry(root).or_default();
        if *count >= AUTO_WAKE_LIMIT {
            if *count == AUTO_WAKE_LIMIT {
                *count += 1;
                drop(wakes);
                self.shared
                    .bus
                    .emit(Event::new(LifecycleEvent::AutoContinuationPaused {
                        run_id: root.to_string(),
                        limit: AUTO_WAKE_LIMIT,
                    }));
            }
            return false;
        }
        *count += 1;
        true
    }

    /// A subagent continues under its live parent's current authority. Without
    /// a live parent the messages stay held until the subagent is resumed.
    fn wake_subagent(&self, parent: RunId, run: RunId) {
        let first = {
            let mut held = self
                .shared
                .held_messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(queue) = held.get_mut(&run) {
                queue.retain(|message| !message.observed());
            }
            match held.get_mut(&run) {
                Some(queue) => match queue.iter().position(|q| {
                    q.question_notice()
                        .is_none_or(|n| !n.paused.load(Ordering::Acquire))
                }) {
                    Some(index) => queue.remove(index),
                    None => return,
                },
                _ => return,
            }
        };
        #[cfg(test)]
        if let Ok(execution) = self.execution_snapshot(run) {
            self.execution_hook(HookPoint::HeldMessageTaken, &execution);
        }
        if let Err(error) = self.restore_queued_and_deliver(parent, run, first.clone()) {
            tracing::info!(%run, %error, "held messages wait for the subagent to be resumed");
            let _runs = lock_runs(&self.shared.runs);
            if first.observed() {
                return;
            }
            self.shared
                .held_messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(run)
                .or_default()
                .insert(0, first);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn observed_only_held_reports_do_not_wake_root_or_restore_subagent() {
        use crate::mailbox::{CompletionNotice, QueuedMessage};
        for subagent in [false, true] {
            let bus = Arc::new(EventBus::new(128));
            let runtime = AgentRuntime::new(
                bus.clone(),
                Arc::new(ToolExecutor::new(bus.clone())),
                Arc::new(crate::escalation_review::support::ScriptedModel::new([])),
            );
            let root = runtime.delegate_background(
                Role::Orchestrator,
                "root".into(),
                RunConfig::default(),
            );
            let parent = if subagent {
                runtime
                    .delegate_background_as_child(
                        root,
                        Role::Worker,
                        "parent",
                        RunConfig::default(),
                    )
                    .unwrap()
            } else {
                root
            };
            lock_runs(&runtime.shared.runs)
                .get(&parent)
                .unwrap()
                .phase_tx
                .send_replace(AgentRunPhase::Done);
            let child = RunId::new(99);
            let message = AgentMessage {
                message_id: "held".into(),
                sender_run_id: child.to_string(),
                recipient_run_id: parent.to_string(),
                kind: AgentMessageKind::Send,
                content: "output".into(),
                reply_to: None,
            };
            let old = CompletionNotice::new(Some(parent), child, 1);
            runtime.hold_queued_message(
                parent,
                QueuedMessage::completion(message.clone(), old.clone()),
            );
            runtime.observe_completion(&old);
            assert!(runtime.shared.held_messages.lock().unwrap()[&parent].is_empty());
            // Inject a stale queued envelope to independently test the wake/release fences.
            runtime
                .shared
                .held_messages
                .lock()
                .unwrap()
                .entry(parent)
                .or_default()
                .push(QueuedMessage::completion(message.clone(), old.clone()));
            let mut events = bus.subscribe();
            runtime.wake(parent).await;
            assert!(events.drain_pending_snapshot().is_empty());
            assert!(runtime.shared.auto_wakes.lock().unwrap().is_empty());
            let mailbox = RunMailbox::new();
            let version = mailbox.subscribe_version();
            runtime
                .shared
                .held_messages
                .lock()
                .unwrap()
                .entry(parent)
                .or_default()
                .push(QueuedMessage::completion(message.clone(), old));
            runtime.release_held_messages(parent, &mailbox);
            assert!(mailbox.is_empty());
            assert!(!version.has_changed().unwrap());
            // An unobserved generation and a same-text explicit Send are still released.
            runtime.hold_queued_message(
                parent,
                QueuedMessage::completion(
                    message.clone(),
                    CompletionNotice::new(Some(parent), child, 2),
                ),
            );
            runtime.hold_message(parent, message.clone());
            runtime.release_held_messages(parent, &mailbox);
            assert_eq!(mailbox.drain_interrupting(), vec![message.clone()]);
            assert_eq!(mailbox.drain_all(), vec![message.clone()]);
            if !subagent {
                runtime.hold_message(parent, message);
                runtime.wake(parent).await;
                assert!(events.drain_pending_snapshot().iter().any(|event| matches!(&event.kind,
                    EventKind::Lifecycle(LifecycleEvent::RootWakeRequested { run_id }) if run_id == &parent.to_string()
                )));
                assert_eq!(
                    runtime.shared.auto_wakes.lock().unwrap().get(&root),
                    Some(&1)
                );
            }
        }
    }

    #[tokio::test]
    async fn the_bookkeeping_readers_follow_holds_wakes_and_resets() {
        let bus = Arc::new(EventBus::new(64));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus)),
            Arc::new(crate::escalation_review::support::ScriptedModel::new([])),
        );
        assert_eq!(
            (runtime.held_message_count(), runtime.auto_wake_total()),
            (0, 0)
        );
        let (root, recipient) = (RunId::new(1), RunId::new(2));
        let message = AgentMessage {
            message_id: "held".into(),
            sender_run_id: root.to_string(),
            recipient_run_id: recipient.to_string(),
            kind: AgentMessageKind::Send,
            content: "output".into(),
            reply_to: None,
        };
        runtime.hold_message(recipient, message.clone());
        runtime.hold_message(recipient, message);
        assert!(runtime.take_auto_wake(root));
        assert_eq!(
            (runtime.held_message_count(), runtime.auto_wake_total()),
            (2, 1)
        );
        runtime.release_held_messages(recipient, &RunMailbox::new());
        runtime.reset_auto_wakes(root);
        assert_eq!(
            (runtime.held_message_count(), runtime.auto_wake_total()),
            (0, 0)
        );
    }

    #[tokio::test]
    async fn automatic_wakes_pause_once_at_the_limit_until_user_input() {
        let bus = Arc::new(EventBus::new(64));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus.clone())),
            Arc::new(crate::escalation_review::support::ScriptedModel::new([])),
        );
        let mut events = bus.subscribe();
        let root = RunId::new(7);
        for _ in 0..AUTO_WAKE_LIMIT {
            assert!(runtime.take_auto_wake(root));
        }
        assert!(!runtime.take_auto_wake(root));
        assert!(!runtime.take_auto_wake(root));
        runtime.reset_auto_wakes(root);
        assert!(runtime.take_auto_wake(root));
        bus.emit(Event::new(LifecycleEvent::BackgroundTaskCompleted {
            task_id: "probe".into(),
        }));
        let mut pauses = 0;
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::Lifecycle(LifecycleEvent::AutoContinuationPaused { run_id, limit }) => {
                    assert_eq!((run_id, limit), (root.to_string(), AUTO_WAKE_LIMIT));
                    pauses += 1;
                }
                EventKind::Lifecycle(LifecycleEvent::BackgroundTaskCompleted { .. }) => break,
                _ => {}
            }
        }
        assert_eq!(pauses, 1, "the pause is announced once per budget");
    }
}

#[cfg(test)]
mod race_tests;
