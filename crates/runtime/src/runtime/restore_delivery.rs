use super::*;
use crate::RunRestoreFailure;
use crate::restore::{RestoredState, RunRestoreDescriptor};

mod registration;
#[cfg(test)]
mod tests;

mod authorization;

impl AgentRuntime {
    pub(super) fn restore_and_deliver(
        &self,
        sender: RunId,
        recipient: RunId,
        message: AgentMessage,
    ) -> Result<MessageDelivery, RuntimeError> {
        self.restore_queued_and_deliver(
            sender,
            recipient,
            crate::mailbox::QueuedMessage::explicit(message),
        )
        .map(|delivery| delivery.expect("explicit messages cannot be observed completions"))
    }

    /// None means an in-flight automatic notice was acknowledged before commit.
    /// Keep its receipt even while waiting for restore_gate or reading storage.
    pub(super) fn restore_queued_and_deliver(
        &self,
        sender: RunId,
        recipient: RunId,
        mut queued: crate::mailbox::QueuedMessage,
    ) -> Result<Option<MessageDelivery>, RuntimeError> {
        let fail = |reason| RuntimeError::RunRestoreFailed {
            run_id: recipient.to_string(),
            reason,
        };
        // Serialize snapshot reads through registration; never acquire this gate under runs.
        let _restore_guard = self.shared.run_store.get().map(|store| {
            store
                .restore_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        self.ensure_no_saved_compaction(recipient)?;
        let mut live_retry_available = true;
        loop {
            let previous = {
                let runs = lock_runs(&self.shared.runs);
                if queued.observed() {
                    return Ok(None);
                }
                if let Some(entry) = runs.get(&recipient) {
                    if is_live(entry) {
                        drop(runs);
                        self.validate_run_mutation(recipient)?;
                        let result =
                            self.try_live_queued_delivery(sender, recipient, queued.clone());
                        match result {
                            Err(RuntimeError::RunTerminated { .. }) if live_retry_available => {
                                live_retry_available = false;
                                continue;
                            }
                            result => return result,
                        }
                    }
                    self.authorize_restore(
                        &runs,
                        sender,
                        (recipient, entry.parent),
                        &queued.message,
                    )?;
                    // A subagent's report to its finished parent is never lost:
                    // it is held until the parent continues, and while the
                    // operator's stop holds the parent is not woken for it.
                    let from_child =
                        runs.get(&sender).and_then(|sender| sender.parent) == Some(recipient);
                    match self.continuation_refusal(recipient, Some(entry)) {
                        Some(RunRestoreFailure::StoppedByOperator) if from_child => {
                            drop(runs);
                            return self.hold_queued_for_parent(sender, recipient, queued);
                        }
                        Some(reason) => return Err(fail(reason)),
                        None => {}
                    }
                    (Some(Arc::clone(&entry.mailbox)), from_child)
                } else {
                    (None, false)
                }
            };
            let (previous, from_child) = previous;
            let store = match self.shared.run_store.get() {
                Some(store) => store,
                None if from_child => {
                    return self.hold_queued_for_parent(sender, recipient, queued);
                }
                None if previous.is_some() => {
                    return Err(fail(RunRestoreFailure::StorageNotConfigured));
                }
                None => return Err(unknown_run(recipient)),
            };
            let identity = store
                .restore_parent(recipient)
                .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?;
            let Some(parent_id) = identity else {
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
            let parent = parent_id
                .as_deref()
                .map(|id| {
                    id.parse::<RunId>().map_err(|_| {
                        fail(RunRestoreFailure::CorruptContext("parent run ID".into()))
                    })
                })
                .transpose()?;
            if previous.is_none() {
                self.authorize_restore(
                    &lock_runs(&self.shared.runs),
                    sender,
                    (recipient, parent),
                    &queued.message,
                )?;
            }
            // Optional ancestry is read before runs is held; bind it to the exact parent ID.
            let durable_parent = parent
                .filter(|parent| !lock_runs(&self.shared.runs).contains_key(parent))
                .and_then(|parent| {
                    store
                        .restore_parent(parent)
                        .ok()
                        .flatten()
                        .and_then(|grandparent| match grandparent {
                            None => Some((parent, false)),
                            Some(id) if id.parse::<RunId>().is_ok() => Some((parent, true)),
                            _ => None,
                        })
                });
            let mut record = store
                .restore_record(recipient)
                .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?
                .ok_or_else(|| fail(RunRestoreFailure::MissingContext))?;
            let mut descriptor: RunRestoreDescriptor = serde_json::from_str(&record.config_json)
                .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?;
            if descriptor.parent_run_id != parent || descriptor.role != record.role {
                return Err(fail(RunRestoreFailure::CorruptContext(
                    "descriptor identity".into(),
                )));
            }
            let role = Role::from_name(&descriptor.role)
                .map_err(|error| fail(RunRestoreFailure::UnsupportedConfig(error.to_string())))?;
            // Only a direct parent renews execution authority, with its own
            // current permit. Any other sender restores strictly from disk.
            let parent_authority = (parent == Some(sender)).then(|| {
                lock_runs(&self.shared.runs)
                    .get(&sender)
                    .and_then(|entry| entry.config.ownership.clone())
            });
            let (mut restored, ownership) = match parent_authority {
                Some(permit) => {
                    let restored =
                        self.renewed_child_state(&record, &descriptor, permit.as_ref())?;
                    let renewed = !(record.restorable && descriptor.restorable);
                    (restored, permit.filter(|_| renewed))
                }
                None => {
                    let disk = if !record.restorable || !descriptor.restorable {
                        Err(fail(RunRestoreFailure::UnsupportedConfig(
                            descriptor
                                .non_restorable_reason
                                .clone()
                                .unwrap_or_else(|| "実行設定".into()),
                        )))
                    } else {
                        RestoredState::from_record(&record).map(|mut restored| {
                            restored.provenance =
                                store.restore_provenance(&record).unwrap_or_else(|_| {
                                    crate::restore::provenance::ConversationProvenance::unknown()
                                });
                            restored
                        })
                    };
                    match disk {
                        Ok(restored) => (restored, None),
                        // Disk alone cannot continue the parent: keep the report
                        // and let the parent be woken under current authority.
                        Err(_) if from_child => {
                            return self.hold_queued_for_parent(sender, recipient, queued);
                        }
                        Err(error) => return Err(error),
                    }
                }
            };
            let _authority = ownership
                .as_ref()
                .map(crate::ownership::OwnerPermit::mutation_guard)
                .transpose()
                .map_err(|_| RuntimeError::StaleOwnership {
                    run_id: recipient.to_string(),
                })?;
            self.reserve_restored_shell_handles(recipient, &restored)?;
            descriptor.restorable = false;
            descriptor.non_restorable_reason = Some("snapshot_consumed".into());
            record.restorable = false;
            record.config_json = serde_json::to_string(&descriptor)
                .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?;
            let config = RunConfig {
                name: descriptor.name,
                interactive: descriptor.interactive,
                keep_alive: descriptor.keep_alive,
                category: descriptor.category,
                load_skills: descriptor.load_skills,
                model_preference: descriptor.model_preference,
                project_root: descriptor.project_root,
                ownership,
                ..RunConfig::default()
            };
            #[cfg(test)]
            if let Ok(execution) = self.execution_snapshot(recipient) {
                self.execution_hook(HookPoint::RestoreMessagePrepared, &execution);
            }
            let mut intents = self
                .shared
                .spawn_intents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut admissions = self
                .shared
                .admissions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut runs = lock_runs(&self.shared.runs);
            // Linearize observation against snapshot consumption and registration.
            if queued.observed() {
                return Ok(None);
            }
            // A complete intervening restore may already be terminal: phase alone is not a generation check.
            match (previous.as_ref(), runs.get(&recipient)) {
                (_, Some(entry)) if is_live(entry) => {
                    drop(runs);
                    drop(admissions);
                    drop(intents);
                    self.validate_run_mutation(recipient)?;
                    let result = self.try_live_queued_delivery(sender, recipient, queued.clone());
                    match result {
                        Err(RuntimeError::RunTerminated { .. }) if live_retry_available => {
                            live_retry_available = false;
                            continue;
                        }
                        result => return result,
                    }
                }
                (Some(old), Some(entry)) if Arc::ptr_eq(old, &entry.mailbox) => {}
                (None, None) => {}
                _ => continue,
            }
            let parent = runs.get(&recipient).map_or(parent, |entry| entry.parent);
            self.authorize_restore(&runs, sender, (recipient, parent), &queued.message)?;
            if let Some(reason) = self.continuation_refusal(recipient, runs.get(&recipient)) {
                return Err(fail(reason));
            }
            self.shared
                .model
                .restore_route_pins(&recipient.to_string(), &descriptor.route_pins);
            let execution = Self::prepare_restored_execution(recipient, parent, &intents)?;
            // The writer only upserts the already-read record; no storage read holds runs.
            let write = restored.provenance.write(record.clone()).map_err(|error| {
                fail(RunRestoreFailure::SnapshotConsumeFailed(error.to_string()))
            })?;
            store.handle.save_snapshot_bundle(&write).map_err(|error| {
                fail(RunRestoreFailure::SnapshotConsumeFailed(format!(
                    "{error:?}"
                )))
            })?;
            Self::install_restored_execution(
                recipient,
                parent,
                &mut intents,
                &mut admissions,
                &execution,
            );
            self.shared.run_ids.observe(recipient);
            queued.message.message_id = format!(
                "msg-{}",
                self.shared.next_message_id.fetch_add(1, Ordering::Relaxed)
            );
            restored.trigger = Some(queued.message.clone());
            self.register_restored(
                &mut runs,
                &intents,
                execution,
                (recipient, parent, role, config, sender),
                restored,
                &queued.message,
                durable_parent,
            )?;
            return Ok(Some((
                queued.message.message_id.clone(),
                queued.message,
                DeliveryDisposition::Restored,
            )));
        }
    }
    /// Keep a subagent's message for its finished parent and wake the parent.
    /// A parent that came back meanwhile receives it directly.
    fn hold_queued_for_parent(
        &self,
        sender: RunId,
        recipient: RunId,
        mut queued: crate::mailbox::QueuedMessage,
    ) -> Result<Option<MessageDelivery>, RuntimeError> {
        let runs = lock_runs(&self.shared.runs);
        if queued.observed() {
            return Ok(None);
        }
        if runs.get(&recipient).is_some_and(is_live) {
            drop(runs);
            return self.try_live_queued_delivery(sender, recipient, queued);
        }
        queued.message.message_id = format!(
            "msg-{}",
            self.shared.next_message_id.fetch_add(1, Ordering::Relaxed)
        );
        self.hold_queued_message(recipient, queued.clone());
        drop(runs);
        self.schedule_wake(recipient);
        Ok(Some((
            queued.message.message_id.clone(),
            queued.message,
            DeliveryDisposition::Held,
        )))
    }
}

fn is_live(entry: &RunEntry) -> bool {
    !entry.mailbox.is_closed()
        && matches!(
            *entry.phase_rx.borrow(),
            AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
        )
}
