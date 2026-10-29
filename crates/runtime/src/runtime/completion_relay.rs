use super::*;

#[cfg(test)]
mod tests;

impl AgentRuntime {
    pub(crate) fn spawn_goal_reviewer(
        &self,
        parent: RunId,
        run_id: RunId,
        prompt: String,
        config: RunConfig,
    ) {
        self.spawn_run_with_handoff(
            run_id,
            Some(parent),
            Role::Reviewer,
            prompt,
            config,
            RunContinuation::Awaited,
            None,
        );
    }
    pub(crate) fn delegate_awaited_child(
        &self,
        parent: RunId,
        task: (Role, String, RunConfig),
    ) -> Result<RunId, RuntimeError> {
        let run_id = self.reserve_child_run_id(parent)?;
        let (role, prompt, config) = task;
        Ok(self.spawn_run_with_handoff(
            run_id,
            Some(parent),
            role,
            prompt,
            config,
            RunContinuation::Awaited,
            None,
        ))
    }

    pub(crate) fn publish_terminal(&self, run_id: RunId, event: LifecycleEvent) {
        let LifecycleEvent::AgentRunStateChanged { to, ref reason, .. } = event else {
            return;
        };
        if !matches!(
            to,
            AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
        ) {
            return;
        }
        // A finished run asks for nothing more; drop its denial streak.
        self.shared.escalation_denials.clear(run_id);
        let mut runs = lock_runs(&self.shared.runs);
        let Some(child) = runs.get_mut(&run_id) else {
            tracing::warn!(%run_id, ?to, "publishing terminal event for run missing from registry");
            // Preserve the normal bus mutation fence and same-ID registration lock.
            self.shared.bus.emit(Event::new(event));
            return;
        };
        let content = match to {
            AgentRunPhase::Done => child
                .result_rx
                .borrow()
                .clone()
                .unwrap_or_else(|| "completed without final output".into()),
            AgentRunPhase::Error => match reason.as_deref() {
                Some("cancelled") => "cancelled".into(),
                Some(reason) => format!("failed: {reason}"),
                None => "failed".into(),
            },
            AgentRunPhase::Stopped => "stopped".into(),
            AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting => return,
        };
        child.terminal_reason = reason.clone();
        // Finalization is complete. Keep cancellation before Error for observers
        // that stop at the terminal phase; the runs lock still fences restarts.
        if !child.completion_relayed
            && to == AgentRunPhase::Error
            && reason.as_deref() == Some("cancelled")
        {
            self.shared
                .bus
                .emit(Event::new(LifecycleEvent::BackgroundTaskCancelled {
                    task_id: run_id.to_string(),
                }));
        }
        child.phase_tx.send_replace(to);
        self.shared.bus.emit(Event::new(event));
        if child.completion_relayed {
            return;
        }
        child.completion_relayed = true;
        // Serialize the legacy notification with terminal publication so a
        // same-ID restart cannot overtake completion of its previous incarnation.
        let legacy = match to {
            AgentRunPhase::Done => Some(LifecycleEvent::BackgroundTaskCompleted {
                task_id: run_id.to_string(),
            }),
            _ => None,
        };
        if let Some(event) = legacy {
            self.shared.bus.emit(Event::new(event));
        }
        let notice = child.completion_notice.clone();
        let Some(parent) = child.parent else { return };
        let Some(recipient) = runs.get(&parent) else {
            return;
        };
        let parent_phase = *recipient.phase_rx.borrow();
        let parent_cancelled = matches!(
            *recipient.execution.cancel_tx.borrow(),
            RunInterrupt::Cancel
        ) || recipient.terminal_reason.as_deref()
            == Some(event_bus::CANCELLED_REASON);
        let message = AgentMessage {
            message_id: format!(
                "msg-{}",
                self.shared.next_message_id.fetch_add(1, Ordering::Relaxed)
            ),
            sender_run_id: run_id.to_string(),
            recipient_run_id: parent.to_string(),
            kind: AgentMessageKind::Send,
            content,
            reply_to: None,
        };
        // A finished or stopped parent keeps the report and is woken for it;
        // a cancelled parent was discarded with its subtree.
        let hold = |runtime: &Self| {
            if !parent_cancelled {
                runtime.hold_queued_message(
                    parent,
                    crate::mailbox::QueuedMessage::completion(message.clone(), notice.clone()),
                );
                runtime.schedule_wake(parent);
            }
        };
        let disposition = match parent_phase {
            AgentRunPhase::Stopped | AgentRunPhase::Done | AgentRunPhase::Error => {
                hold(self);
                return;
            }
            AgentRunPhase::Waiting => DeliveryDisposition::Wake,
            AgentRunPhase::Pending | AgentRunPhase::Running => DeliveryDisposition::Aside,
        };
        match recipient
            .mailbox
            .try_push_completion(message.clone(), notice.clone())
        {
            Ok(()) => {
                self.shared
                    .sent
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(
                        message.message_id.clone(),
                        SentRecord {
                            sender: run_id,
                            recipient: parent,
                        },
                    );
                self.shared
                    .bus
                    .emit(Event::new(AgentMessageEvent::Delivered {
                        message,
                        disposition,
                    }));
            }
            Err(PushError::Full) => {
                tracing::warn!(child = %run_id, %parent, "completion relay dropped: parent mailbox full")
            }
            // The parent is finishing; keep the report for when it continues.
            Err(PushError::Closed) => hold(self),
        }
    }
}
