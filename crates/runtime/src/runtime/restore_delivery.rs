use super::*;
use crate::RunRestoreFailure;
use crate::restore::{RestoredState, RunRestoreDescriptor};

mod registration;

impl AgentRuntime {
    pub(super) fn restore_and_deliver(
        &self,
        sender: RunId,
        recipient: RunId,
        mut message: AgentMessage,
    ) -> Result<(String, AgentMessage, DeliveryDisposition), RuntimeError> {
        let fail = |reason| RuntimeError::RunRestoreFailed {
            run_id: recipient.to_string(),
            reason,
        };
        loop {
            let previous = {
                let runs = lock_runs(&self.shared.runs);
                if let Some(entry) = runs.get(&recipient) {
                    if is_live(entry) {
                        drop(runs);
                        self.validate_run_mutation(recipient)?;
                        return self.prepare_delivery(
                            sender,
                            recipient,
                            message.kind,
                            message.content,
                            message.reply_to,
                        );
                    }
                    self.authorize_restore(&runs, sender, (recipient, entry.parent), &message)?;
                    Some(Arc::clone(&entry.mailbox))
                } else {
                    None
                }
            };
            let store = self.shared.run_store.get().ok_or_else(|| {
                if previous.is_some() {
                    fail(RunRestoreFailure::StorageNotConfigured)
                } else {
                    unknown_run(recipient)
                }
            })?;
            let record = store
                .restore_record(recipient)
                .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?;
            let Some(record) = record else {
                let known = previous.is_some()
                    || !store
                        .ledger_entries(recipient)
                        .map_err(|error| {
                            fail(RunRestoreFailure::CorruptContext(error.to_string()))
                        })?
                        .is_empty();
                return Err(if known {
                    fail(RunRestoreFailure::MissingContext)
                } else {
                    unknown_run(recipient)
                });
            };
            let parent = record
                .parent_run_id
                .as_deref()
                .map(|id| {
                    id.strip_prefix("run-")
                        .and_then(|id| id.parse::<u64>().ok())
                        .map(RunId::new)
                        .ok_or_else(|| {
                            fail(RunRestoreFailure::CorruptContext("parent run ID".into()))
                        })
                })
                .transpose()?;
            if previous.is_none() {
                self.authorize_restore(
                    &lock_runs(&self.shared.runs),
                    sender,
                    (recipient, parent),
                    &message,
                )?;
            }
            let descriptor: RunRestoreDescriptor = serde_json::from_str(&record.config_json)
                .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?;
            if !record.restorable || !descriptor.restorable {
                return Err(fail(RunRestoreFailure::UnsupportedConfig(
                    descriptor
                        .non_restorable_reason
                        .unwrap_or_else(|| "実行設定".into()),
                )));
            }
            if descriptor.parent_run_id != parent || descriptor.role != record.role {
                return Err(fail(RunRestoreFailure::CorruptContext(
                    "descriptor identity".into(),
                )));
            }
            let role = Role::from_name(&descriptor.role)
                .map_err(|error| fail(RunRestoreFailure::UnsupportedConfig(error.to_string())))?;
            let messages: Vec<providers::Message> = serde_json::from_str(&record.messages_json)
                .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?;
            let checkpoints: Vec<crate::CompactionCheckpoint> =
                serde_json::from_str(&record.checkpoints_json)
                    .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?;
            if messages.is_empty()
                || checkpoints.iter().any(|checkpoint| {
                    checkpoint.range.0 >= checkpoint.range.1 || checkpoint.range.1 > messages.len()
                })
            {
                return Err(fail(RunRestoreFailure::CorruptContext(
                    "context range".into(),
                )));
            }
            let next_id = recipient.get().checked_add(1).ok_or_else(|| {
                fail(RunRestoreFailure::UnsupportedConfig(
                    "run ID overflow".into(),
                ))
            })?;
            let config = RunConfig {
                name: descriptor.name,
                interactive: descriptor.interactive,
                keep_alive: descriptor.keep_alive,
                category: descriptor.category,
                load_skills: descriptor.load_skills,
                workspace_mode: descriptor.workspace_mode,
                network_access: descriptor.network_access,
                model_preference: descriptor.model_preference,
                ..RunConfig::default()
            };
            let mut runs = lock_runs(&self.shared.runs);
            // A complete intervening restore may already be terminal: phase alone is not a generation check.
            match (previous.as_ref(), runs.get(&recipient)) {
                (_, Some(entry)) if is_live(entry) => {
                    drop(runs);
                    self.validate_run_mutation(recipient)?;
                    return self.prepare_delivery(
                        sender,
                        recipient,
                        message.kind,
                        message.content,
                        message.reply_to,
                    );
                }
                (Some(old), Some(entry)) if Arc::ptr_eq(old, &entry.mailbox) => {}
                (None, None) => {}
                _ => continue,
            }
            let parent = runs.get(&recipient).map_or(parent, |entry| entry.parent);
            self.authorize_restore(&runs, sender, (recipient, parent), &message)?;
            self.shared
                .next_run_id
                .fetch_max(next_id, Ordering::Relaxed);
            message.message_id = format!(
                "msg-{}",
                self.shared.next_message_id.fetch_add(1, Ordering::Relaxed)
            );
            let restored = RestoredState {
                messages,
                checkpoints,
                trigger: message.clone(),
            };
            self.register_restored(
                &mut runs,
                (recipient, parent, role, config, sender),
                restored,
            );
            return Ok((
                message.message_id.clone(),
                message,
                DeliveryDisposition::Restored,
            ));
        }
    }

    fn authorize_restore(
        &self,
        runs: &HashMap<RunId, RunEntry>,
        sender: RunId,
        recipient: (RunId, Option<RunId>),
        message: &AgentMessage,
    ) -> Result<(), RuntimeError> {
        let sender_entry = runs.get(&sender).ok_or_else(|| unknown_run(sender))?;
        let (recipient, parent) = recipient;
        let denied = |detail: &str| RuntimeError::MessageDenied {
            sender,
            recipient,
            detail: detail.into(),
        };
        if sender == recipient {
            return Err(denied("自己宛てのメッセージは許可されていません"));
        }
        if message.kind == AgentMessageKind::Steering && parent != Some(sender) {
            return Err(denied("steering は親から子へのみ許可されています"));
        }
        if sender_entry.parent != Some(recipient) && parent != Some(sender) {
            return Err(denied(
                "親子関係のない run 間のメッセージは許可されていません",
            ));
        }
        if message.kind == AgentMessageKind::Reply {
            let id = message
                .reply_to
                .as_ref()
                .ok_or_else(|| denied("Reply には reply_to が必要です"))?;
            let sent = self
                .shared
                .sent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !sent
                .get(id)
                .is_some_and(|record| record.recipient == sender && record.sender == recipient)
            {
                return Err(RuntimeError::UnknownMessage {
                    message_id: id.clone(),
                });
            }
        }
        Ok(())
    }
}

fn is_live(entry: &RunEntry) -> bool {
    matches!(
        *entry.phase_rx.borrow(),
        AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
    )
}
