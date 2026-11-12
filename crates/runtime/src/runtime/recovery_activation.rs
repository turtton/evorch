//! Authority-free prepare/release. A paused claim is never an executable run.
use super::*;
use crate::quiesce::{
    RecoveryClaim, RecoveryClaimState, RecoveryComponent, ResumeIntent, RunRecoveryRecord,
};
use std::collections::BTreeSet;
use storage::recovery::RecoveryClaimRequest;

mod activate;
mod adopt;
mod cleanup;
pub use activate::CurrentRecoveryAuthority;
pub use cleanup::{RecoveryClaimHandle, RecoveryCleanupHandle, RecoveryCleanupState};
mod models;
mod relations;
pub use models::RecoveryModels;

struct RecoverySource {
    record: RunRecoveryRecord,
    intent: ResumeIntent,
    bundle: Option<storage::thread_index::SnapshotBundle>,
    snapshot: Option<storage::RunContextReceipt>,
}

pub(super) struct PausedRecoveryIndex {
    pub(super) claim_id: String,
    pub(super) old_incarnation: String,
    // Only validated adoption may supply current Stop authority.
    stop_config: Option<RunConfig>,
    startup: Arc<storage::RecoveryStartupComplete>,
    ownership_root: std::path::PathBuf,
}

/// Owns one prepared component's process-local reservations. Dropping it does
/// not release the durable claim or undo any revision; startup sees PendingClaim.
/// No authority, task, provider admission or wake can be obtained from this type.
pub struct PausedRecoveryComponent {
    runtime: Weak<Shared>,
    claim: RecoveryClaim,
    members: Vec<ResumeIntent>,
    controls: Vec<(RunId, Arc<ExecutionControl>)>,
    sources: Vec<RecoverySource>,
    relations: Option<relations::PreparedRelations>,
    _reservations: Vec<SnapshotReservation>,
    _startup: Arc<storage::RecoveryStartupComplete>,
    #[cfg(test)]
    lose_activation_ack: bool,
}
impl PausedRecoveryComponent {
    #[cfg(test)]
    pub(crate) fn lose_activation_ack_for_test(&mut self) {
        self.lose_activation_ack = true;
    }
    pub fn claim(&self) -> &RecoveryClaim {
        &self.claim
    }
}

fn error(reason: impl ToString) -> RuntimeError {
    crate::quiesce::runtime::error(reason)
}

impl AgentRuntime {
    pub(super) fn fence_paused_stop(
        &self,
        run: RunId,
        old: &str,
        reason: crate::quiesce::StopReason,
    ) -> Result<(), RuntimeError> {
        let (claim, permit) = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run)
            .and_then(|i| i.paused_recovery.as_ref())
            .map(|p| {
                (
                    p.claim_id.clone(),
                    p.stop_config.as_ref().and_then(|c| c.ownership.clone()),
                )
            })
            .ok_or_else(|| error("paused recovery Stop index missing"))?;
        let permit =
            permit.ok_or_else(|| error("paused recovery Stop requires current authority"))?;
        // Pin this owner generation from re-observation through the durable CAS.
        let _authority = permit.mutation_guard().map_err(error)?;
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let snapshot = store
            .with_database(|db| db.recovery_claim_snapshot(&claim))
            .map_err(error)?;
        if !cleanup::prepared_stoppable(&snapshot)
            || snapshot
                .intents
                .iter()
                .find(|i| i.run_id == run.to_string())
                .and_then(|i| i.intent.as_ref())
                .is_none_or(|i| i.incarnation != old)
        {
            return Err(error(
                "paused recovery requires quarantine cleanup, not Stop",
            ));
        }
        self.shared
            .quiesce
            .update(run, old, self.shared.run_store.get(), |intent| {
                intent.active = false;
                intent.resume_requested = false;
                intent.stop_reason = Some(reason);
            })
            .map_err(error)
    }
    pub fn has_paused_recovery(&self) -> bool {
        !self.paused_recovery_run_ids().is_empty()
    }
    pub(crate) fn paused_recovery_run_ids(&self) -> Vec<RunId> {
        self.shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter_map(|(run, index)| index.paused_recovery.as_ref().map(|_| *run))
            .collect()
    }

    /// Prepare only the currently validated component, after the exclusive
    /// startup phase. Saved recipes never manufacture a RunConfig or permit.
    pub fn prepare_component(
        &self,
        component: &RecoveryComponent,
        startup: storage::RecoveryStartupComplete,
        ownership_root: &std::path::Path,
    ) -> Result<PausedRecoveryComponent, RuntimeError> {
        let _dispatch = self.dispatch_registration_guard()?;
        let startup = Arc::new(startup);
        if !self
            .shared
            .recovery_message_ids_safe
            .load(Ordering::Acquire)
        {
            return Err(error(
                "run store must be attached before issuing message IDs for recovery",
            ));
        }
        if !self.recovery_plan()?.components.contains(component) {
            return Err(error("component is no longer a recovery candidate"));
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let manifest = store
            .with_database(|db| {
                db.recovery_manifests()?
                    .into_iter()
                    .find(|m| m.manifest_id == component.manifest_id)
                    .ok_or_else(|| storage::StorageError::Io("missing recovery manifest".into()))?
                    .record
                    .map_err(|_| storage::StorageError::Io("corrupt recovery manifest".into()))
            })
            .map_err(error)?;
        let mut ids = manifest
            .members
            .iter()
            .map(|m| m.run_id.parse::<RunId>().map_err(error))
            .collect::<Result<Vec<_>, _>>()?;
        ids.sort_unstable();
        let gates = ids
            .iter()
            .map(|id| self.stop_restore_gate(*id))
            .collect::<Vec<_>>();
        let _stop = gates
            .iter()
            .map(|gate| {
                gate.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            })
            .collect::<Vec<_>>();
        let mut indexes = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        {
            let admissions = self
                .shared
                .admissions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let runs = lock_runs(&self.shared.runs);
            if ids.iter().any(|id| {
                indexes.get(id).is_some_and(|i| {
                    i.execution.is_some() || i.paused_recovery.is_some() || i.cancelled
                }) || admissions.contains_key(id)
                    || runs.contains_key(id)
            }) {
                return Err(error(
                    "recovery member already registered, reserved or cancelled",
                ));
            }
        }
        let mut sources = Vec::new();
        let mut incarnations = Vec::new();
        for member in &manifest.members {
            let record: RunRecoveryRecord =
                serde_json::from_str(&member.record_json).map_err(error)?;
            if record.next_message_id > store.next_recovery_message_id {
                return Err(error(
                    "component message IDs were not fenced at run store startup",
                ));
            }
            let (intent, snapshot) = store
                .with_database(|db| {
                    Ok((
                        db.resume_intent(&member.run_id)?.ok_or_else(|| {
                            storage::StorageError::Io("missing recovery intent".into())
                        })?,
                        db.snapshot_bundle(&member.run_id)
                            .map_err(crate::restore::provenance::storage_error)?,
                    ))
                })
                .map_err(error)?;
            let intent = ResumeIntent::decode(intent).map_err(error)?;
            let execution = member.resume.then(|| ExecutionControl::new(false));
            if let Some(execution) = &execution {
                incarnations.push((member.run_id.clone(), execution.publication().incarnation()));
            }
            sources.push((record, intent, snapshot, member.snapshot.clone(), execution));
        }
        // Validate process-local tokens before the durable claim. No later
        // prepare step is fallible or writes a snapshot consumption marker.
        let mut reservations = Vec::new();
        for (record, _, snapshot, receipt, _) in &sources {
            if let Some(receipt) = receipt {
                let snapshot = snapshot
                    .as_ref()
                    .ok_or_else(|| error("missing recovery snapshot"))?;
                reservations.push(SnapshotReservation::prepare_recovery(
                    store,
                    record.run_id,
                    &snapshot.record,
                    receipt,
                )?);
            }
        }
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).map_err(error)?;
        let claim = store
            .handle
            .claim_recovery_component(
                RecoveryClaimRequest {
                    claim_id: format!("recovery-{:032x}", u128::from_le_bytes(nonce)),
                    manifest_id: component.manifest_id.clone(),
                    incarnations,
                },
                &startup,
                ownership_root,
            )
            .map_err(error)?;
        let members = sources
            .iter()
            .map(|(_, intent, _, _, _)| intent.clone())
            .collect::<Vec<_>>();
        let controls = sources
            .iter()
            .filter_map(|(record, _, _, _, execution)| {
                execution
                    .as_ref()
                    .map(|execution| (record.run_id, execution.clone()))
            })
            .collect();
        self.shared.quiesce.adopt(members.clone(), true);
        let mut retained = Vec::new();
        for (record, intent, bundle, snapshot, execution) in sources {
            retained.push(RecoverySource {
                record: record.clone(),
                intent: intent.clone(),
                bundle,
                snapshot,
            });
            let index = indexes.entry(record.run_id).or_default();
            index.parent = record.parent;
            index.execution = execution;
            index.paused_recovery = Some(PausedRecoveryIndex {
                claim_id: claim.claim_id.clone(),
                old_incarnation: intent.incarnation,
                stop_config: None,
                startup: Arc::clone(&startup),
                ownership_root: ownership_root.to_path_buf(),
            });
        }
        Ok(PausedRecoveryComponent {
            runtime: Arc::downgrade(&self.shared),
            claim,
            members,
            controls,
            sources: retained,
            relations: None,
            _reservations: reservations,
            _startup: startup,
            #[cfg(test)]
            lose_activation_ack: false,
        })
    }

    /// Only an unactivated claim may be released. A failed/uncertain ACK retains
    /// the index unless a read-only claim read proves that release committed.
    pub fn release_prepared_component(
        &self,
        paused: &PausedRecoveryComponent,
        reason: &str,
    ) -> Result<(), RuntimeError> {
        if !Weak::ptr_eq(&paused.runtime, &Arc::downgrade(&self.shared)) {
            return Err(error("foreign paused recovery component"));
        }
        let expected = paused
            .members
            .iter()
            .map(|i| i.run_id)
            .collect::<BTreeSet<_>>();
        self.release_indexed_recovery(&paused.claim.claim_id, reason, Some(&expected), false)
    }

    fn release_indexed_recovery(
        &self,
        claim_id: &str,
        reason: &str,
        expected: Option<&BTreeSet<RunId>>,
        require_current_stop: bool,
    ) -> Result<(), RuntimeError> {
        let members = {
            let indexes = self
                .shared
                .spawn_intents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut members = indexes
                .iter()
                .filter_map(|(id, index)| {
                    index
                        .paused_recovery
                        .as_ref()
                        .filter(|p| p.claim_id == claim_id)
                        .map(|p| (*id, p.old_incarnation.clone(), index.execution.clone()))
                })
                .collect::<Vec<_>>();
            members.sort_unstable_by_key(|(id, _, _)| *id);
            members
        };
        let ids = members
            .iter()
            .map(|(id, _, _)| *id)
            .collect::<BTreeSet<_>>();
        if ids.is_empty() || expected.is_some_and(|expected| *expected != ids) {
            return Err(error("paused recovery index changed"));
        }
        let gates = ids
            .iter()
            .map(|id| self.stop_restore_gate(*id))
            .collect::<Vec<_>>();
        let _stop = gates
            .iter()
            .map(|gate| {
                gate.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            })
            .collect::<Vec<_>>();
        let _publication = members
            .iter()
            .filter_map(|(_, _, execution)| execution.as_ref().map(|e| e.publication_gate()))
            .collect::<Vec<_>>();
        let mut indexes = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if members.iter().any(|(id, old, execution)| {
            indexes.get(id).is_none_or(|index| {
                index
                    .paused_recovery
                    .as_ref()
                    .is_none_or(|p| p.claim_id != claim_id || p.old_incarnation != *old)
                    || match (&index.execution, execution) {
                        (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                        (None, None) => false,
                        _ => true,
                    }
            })
        }) {
            return Err(error("paused recovery index changed"));
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let permits = if require_current_stop {
            members
                .iter()
                .filter(|(_, _, e)| e.is_some())
                .map(|(id, _, _)| {
                    indexes
                        .get(id)
                        .and_then(|i| i.paused_recovery.as_ref())
                        .and_then(|p| p.stop_config.as_ref())
                        .and_then(|c| c.ownership.clone())
                        .ok_or_else(|| error("shutdown release requires current authority"))
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        let _authority = crate::ownership::OwnerPermit::mutation_guards(&permits).map_err(error)?;
        if let Err(write_error) = store
            .handle
            .release_recovery_claim(claim_id.into(), reason.into())
        {
            let released = store
                .with_database(|db| db.recovery_claims())
                .map_err(error)?
                .into_iter()
                .any(|c| {
                    c.claim_id == claim_id
                        && c.state
                            == RecoveryClaimState::Released {
                                reason: reason.into(),
                            }
                });
            if !released {
                return Err(error(write_error));
            }
        }
        for (id, old, execution) in &members {
            if let Some(execution) = execution {
                execution.stop_admission();
            }
            self.shared.quiesce.forget_released(*id, old);
            indexes.remove(id);
        }
        Ok(())
    }

    /// Operator shutdown only; planned restart deliberately carries Prepared to
    /// the next process, where 06c must resolve it under current authority.
    pub(crate) fn release_shutdown_recovery(&self) -> Result<(), RuntimeError> {
        let claims = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter_map(|i| i.paused_recovery.as_ref().map(|p| p.claim_id.clone()))
            .collect::<BTreeSet<_>>();
        let mut failure = None;
        for claim in claims {
            let evidence = {
                let indexes = self
                    .shared
                    .spawn_intents
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let entries = indexes
                    .iter()
                    .filter_map(|(id, i)| {
                        i.paused_recovery
                            .as_ref()
                            .filter(|p| p.claim_id == claim)
                            .map(|p| (*id, p))
                    })
                    .collect::<Vec<_>>();
                entries.first().map(|(_, p)| {
                    (
                        Arc::clone(&p.startup),
                        p.ownership_root.clone(),
                        CurrentRecoveryAuthority {
                            members: entries
                                .iter()
                                .filter_map(|(id, p)| {
                                    p.stop_config.clone().map(|config| (*id, config))
                                })
                                .collect(),
                        },
                    )
                })
            };
            let result = evidence
                .ok_or_else(|| error("shutdown recovery evidence missing"))
                .and_then(|(startup, root, authority)| {
                    self.recover_recovery_claim(
                        &claim,
                        startup.try_clone().map_err(error)?,
                        &root,
                        &authority,
                    )
                })
                .and_then(|handle| match handle {
                    RecoveryClaimHandle::Retryable(paused) => {
                        paused.claim.members.iter().try_for_each(|m| {
                            self.stop(m.run_id.parse().map_err(error)?, crate::StopScope::SelfOnly)
                        })?;
                        self.release_indexed_recovery(&claim, "shutdown", None, true)
                    }
                    RecoveryClaimHandle::Quarantined(handle) => {
                        let snapshot = self
                            .shared
                            .run_store
                            .get()
                            .ok_or_else(|| error("no run store"))?
                            .with_database(|db| db.recovery_claim_snapshot(&claim))
                            .map_err(error)?;
                        if cleanup::prepared_explicitly_stopped(&snapshot) {
                            // Diagnostic selection is not authority. This path
                            // rechecks every current owner and the writer proves
                            // all members unexecuted (including explicit Stop)
                            // in its FULL transaction. Never make this Retryable.
                            self.release_indexed_recovery(&claim, "shutdown after Stop", None, true)
                        } else {
                            self.cleanup_recovery_claim(&handle)
                        }
                    }
                });
            if let Err(err) = result {
                // End local control even when durable terminality is unproven.
                if let Ok(handle) = self.recovery_cleanup_handle(&claim) {
                    let _ = self.cleanup_recovery_claim(&handle);
                }
                failure.get_or_insert(err);
            }
        }
        failure.map_or(Ok(()), Err)
    }
}
