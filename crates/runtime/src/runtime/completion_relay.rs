use super::*;

#[cfg(test)]
mod tests;

impl AgentRuntime {
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
        ))
    }

    pub(crate) fn publish_terminal(&self, run_id: RunId, event: LifecycleEvent) {
        let LifecycleEvent::AgentRunStateChanged { to, ref reason, .. } = event else {
            return;
        };
        let mut runs = lock_runs(&self.shared.runs);
        let Some(child) = runs.get_mut(&run_id) else {
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
        let Some(parent) = child.parent else { return };
        let Some(recipient) = runs.get(&parent) else {
            return;
        };
        let disposition = match *recipient.phase_rx.borrow() {
            AgentRunPhase::Done | AgentRunPhase::Error => return,
            AgentRunPhase::Waiting => DeliveryDisposition::Wake,
            AgentRunPhase::Pending | AgentRunPhase::Running => DeliveryDisposition::Aside,
        };
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
        match recipient.mailbox.try_push(message.clone()) {
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
            Err(PushError::Closed) => {}
        }
    }
}
