//! Messages for a finished or stopped run wait until it is registered again.
//! A subagent's report wakes its parent unless the operator stopped it.
//! Automatic wakes without user input are bounded per conversation so that
//! agents cannot keep each other running indefinitely.

use super::*;

/// Automatic wakes allowed in one conversation between user inputs.
pub(crate) const AUTO_WAKE_LIMIT: u32 = 20;

impl AgentRuntime {
    /// Hold `message` for a recipient that is not running. The caller holds the
    /// runs lock, so registration cannot release the queue concurrently.
    pub(super) fn hold_message(&self, recipient: RunId, message: AgentMessage) {
        self.shared
            .held_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(recipient)
            .or_default()
            .push(message.clone());
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
        for message in held {
            let sender = message.sender_run_id.parse::<RunId>().ok();
            match mailbox.try_push_completion(message.clone()) {
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
                    tracing::warn!(%run_id, ?error, "held message could not be released")
                }
            }
        }
    }

    /// The user spoke to this conversation: automatic wakes may continue.
    pub(crate) fn reset_auto_wakes(&self, root: RunId) {
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

    async fn wake(&self, run: RunId) {
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
        let pending = self
            .shared
            .held_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run)
            .is_some_and(|queue| !queue.is_empty());
        if !pending {
            return;
        }
        let (parent, root) = {
            let runs = lock_runs(&self.shared.runs);
            let Some(entry) = runs.get(&run) else {
                return;
            };
            let mut root = run;
            let mut visited = HashSet::from([run]);
            while let Some(parent) = runs.get(&root).and_then(|entry| entry.parent) {
                if !visited.insert(parent) {
                    break;
                }
                root = parent;
            }
            (entry.parent, root)
        };
        if !self.take_auto_wake(root) {
            return;
        }
        match parent {
            None => {
                self.shared
                    .bus
                    .emit(Event::new(LifecycleEvent::RootWakeRequested {
                        run_id: run.to_string(),
                    }));
            }
            Some(parent) => self.wake_subagent(parent, run),
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
            match held.get_mut(&run) {
                Some(queue) if !queue.is_empty() => queue.remove(0),
                _ => return,
            }
        };
        if let Err(error) = self.restore_and_deliver(parent, run, first.clone()) {
            tracing::info!(%run, %error, "held messages wait for the subagent to be resumed");
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
