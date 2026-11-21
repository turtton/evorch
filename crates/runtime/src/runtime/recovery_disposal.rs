//! Non-executable disposal. Main revocation always precedes owner cleanup.
use super::*;
use crate::scratch::usage::DisposalScratchUse;
use std::collections::{BTreeMap, BTreeSet};
use storage::recovery::{
    graph_control::{GraphControlCas, GraphControlRecord, GraphEffectKey},
    graph_progress::{GraphDisposalProgressKey, GraphDisposalStep, GraphThreadProgressWrite},
};

/// Memory-only admission, opaque and non-Clone. Not executable authority.
///
/// ```no_run
/// fn reserve(rt: &runtime::AgentRuntime, effect: storage::recovery::graph_control::GraphEffectKey) {
///     let _: runtime::runtime::recovery_disposal::DisposalReservation =
///         rt.reserve_owned_graph_disposal("graph", effect, 0).unwrap();
/// }
/// ```
///
/// ```compile_fail
/// fn execute(token: runtime::runtime::recovery_disposal::DisposalReservation) {
///     let _: runtime::ManualActivationReservation = token.into();
/// }
/// ```
pub struct DisposalReservation(ActivationReservation, String);
impl DisposalReservation {
    pub fn confirm_graph_observation(&self) {
        self.0.confirm_graph_observation();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisposalThreadResult {
    pub thread_id: String,
    pub owners: GraphDisposalStep,
    pub scratch: GraphDisposalStep,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisposalResult {
    pub control: Option<GraphControlRecord>,
    pub threads: Vec<DisposalThreadResult>,
    pub unknown: bool,
}
impl DisposalResult {
    pub fn complete(&self) -> bool {
        self.control.as_ref().is_some_and(|c| c.revoked)
            && !self.unknown
            && !self.threads.is_empty()
            && self.threads.iter().all(|t| {
                matches!(
                    t.owners,
                    GraphDisposalStep::Applied | GraphDisposalStep::NotApplicable
                ) && matches!(
                    t.scratch,
                    GraphDisposalStep::Applied | GraphDisposalStep::NotApplicable
                )
            })
    }
}
impl AgentRuntime {
    /// Persist manual execution policy using execution's current permits. This
    /// never accepts or manufactures DisposalAuthority.
    pub fn begin_owned_graph_resume(
        &self,
        expected: &GraphControlCas,
        permits: &[crate::ownership::OwnerPermit],
        reservation: &ActivationReservation,
    ) -> Result<GraphControlRecord, RuntimeError> {
        reservation
            .validate(self, &expected.manifest.manifest_id)
            .map_err(|_| RuntimeError::GraphControlConflict)?;
        let storage::recovery::graph_control::ActivationControl::Manual {
            effect,
            control_revision,
        } = reservation.control()
        else {
            return Err(RuntimeError::GraphControlConflict);
        };
        if expected.control_revision.checked_add(1) != Some(control_revision)
            || !matches!(
                self.reserved_owned_graph_ownership_diagnostic(expected, reservation)
                    .map_err(|_| RuntimeError::GraphControlNotApplied)?
                    .state(),
                OwnedGraphOwnershipState::LiveSelf
            )
        {
            return Err(RuntimeError::GraphControlConflict);
        }
        let _owners = crate::ownership::disposal::guard_resume(expected, permits)
            .map_err(|_| RuntimeError::GraphControlConflict)?;
        let store = self
            .shared
            .run_store
            .get()
            .ok_or(RuntimeError::GraphControlNotApplied)?;
        super::disposal_support::graph_control_write_result(
            store.handle.write_recovery_graph_control(
                storage::recovery::graph_control::GraphControlWrite {
                    expected: expected.clone(),
                    effect: effect.clone(),
                    action: storage::recovery::graph_control::GraphControlAction::Resume,
                },
            ),
            || store.with_database(|db| db.recovery_graph_effect(&effect.operation_id)),
            |c| {
                c.effect == effect
                    && c.manifest_id == expected.manifest.manifest_id
                    && c.revision == control_revision
                    && !c.revoked
                    && c.mode == "manual_running"
            },
        )
    }

    /// No database, owner registry, provider, binding, or filesystem access.
    pub fn reserve_owned_graph_disposal(
        &self,
        manifest: &str,
        effect: GraphEffectKey,
        revision: u64,
    ) -> Result<DisposalReservation, RuntimeError> {
        Ok(DisposalReservation(
            self.reserve_manual_activation(manifest, effect, revision)?
                .into(),
            manifest.to_owned(),
        ))
    }

    /// Readback by the ORIGINAL finished job. A late eligibility ACK may
    /// settle policy, but never authorizes another cleanup or scratch reset.
    pub fn reconcile_owned_graph_disposal(
        &self,
        effect: &GraphEffectKey,
        reservation: &DisposalReservation,
    ) -> Result<bool, RuntimeError> {
        let manifest = &reservation.1;
        reservation.0.validate(self, manifest)?;
        let storage::recovery::graph_control::ActivationControl::Manual {
            effect: reserved,
            control_revision,
        } = reservation.0.control()
        else {
            return Err(error("disposal reservation required"));
        };
        if reserved != *effect {
            return Err(error("disposal operation changed"));
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let read = || store.with_database(|db| db.recovery_graph_effect(&effect.operation_id));
        let Some(control) = read().map_err(error)?.filter(|c| {
            c.effect == *effect
                && c.manifest_id == *manifest
                && c.revision >= control_revision
                && c.revoked
        }) else {
            return Ok(false);
        };
        if control.mode == "revoked_idle" {
            return Ok(true);
        }
        if control.mode != "revoked_running" {
            return Ok(false);
        }
        match store.handle.finish_recovery_graph_control(control) {
            Ok(c) => Ok(c.effect == *effect && c.revoked && c.mode == "revoked_idle"),
            Err(_) => Ok(read()
                .map_err(error)?
                .is_some_and(|c| c.effect == *effect && c.revoked && c.mode == "revoked_idle")),
        }
    }

    /// Only the ORIGINAL supervisor calls this. It retains the token on
    /// unknown. Query/replay cannot use this as an effect continuation.
    pub fn dispose_owned_graph(
        &self,
        authority: &DisposalAuthority,
        effect: GraphEffectKey,
        reservation: &DisposalReservation,
    ) -> Result<DisposalResult, RuntimeError> {
        reservation
            .0
            .validate(self, &authority.expected().manifest.manifest_id)?;
        let expected_control = storage::recovery::graph_control::ActivationControl::Manual {
            effect: effect.clone(),
            control_revision: authority
                .expected()
                .control_revision
                .checked_add(1)
                .ok_or_else(|| error("control revision exhausted"))?,
        };
        if reservation.0.control() != expected_control {
            return Err(error("disposal reservation identity conflict"));
        }
        let control = match self.release_owned_graph_eligibility(authority, effect.clone()) {
            Ok(control) => control,
            Err(
                error @ (RuntimeError::GraphControlConflict | RuntimeError::GraphControlNotApplied),
            ) => return Err(error),
            Err(_) => {
                return Ok(DisposalResult {
                    control: None,
                    threads: vec![],
                    unknown: true,
                });
            }
        };
        let scratch = self
            .shared
            .thread_scratch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        // Scratch snapshot BEFORE supplemental reads; later publication must
        // advance this same epoch before it becomes visible.
        let snapshots = authority
            .permits()
            .keys()
            .map(|thread| {
                (
                    thread.clone(),
                    scratch
                        .as_ref()
                        .map_or(DisposalScratchUse::Unknown, |s| s.disposal_usage(thread)),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let mut shared = self.disposal_runtime_users(authority.expected())?;
        shared.extend(
            store
                .with_database(|db| db.owned_graph_shared_threads(authority.expected()))
                .map_err(error)?,
        );
        shared.retain(|thread| authority.permits().contains_key(thread));
        for (thread, observed) in &snapshots {
            if matches!(observed, DisposalScratchUse::InUse) {
                shared.insert(thread.clone());
            }
        }
        let receipt = self
            .cleanup_owned_graph_receipts(authority, effect.clone(), &shared)
            .or_else(|_| {
                crate::ownership::disposal_support::read(
                    authority.registry_path(),
                    &effect.operation_id,
                )
                .map_err(error)?
                .filter(|r| {
                    r.effect == effect
                        && r.manifest_id == control.manifest_id
                        && r.restart_id == authority.expected().restart_id
                        && r.capture_id == authority.expected().capture_id
                })
                .ok_or_else(|| error("owner cleanup unknown"))
            });
        let mut result = DisposalResult {
            control: Some(control.clone()),
            threads: vec![],
            unknown: false,
        };
        let receipt = match receipt {
            Ok(r) => r,
            Err(_) => {
                result.unknown = true;
                return Ok(result);
            }
        };
        for (thread, permit) in authority.permits() {
            let key = GraphDisposalProgressKey {
                manifest_id: control.manifest_id.clone(),
                restart_id: authority.expected().restart_id.clone(),
                capture_id: authority.expected().capture_id.clone(),
                thread_id: thread.clone(),
                owner_generation: permit.as_ref().map_or(0, |p| p.lease.generation),
            };
            let prior = store
                .with_database(|db| db.recovery_graph_thread_progress(&key))
                .map_err(error)?;
            let owners = if permit.is_none() || receipt.shared_threads.contains(thread) {
                GraphDisposalStep::NotApplicable
            } else {
                GraphDisposalStep::Applied
            };
            let scratch_step = if let Some(prior) = prior.as_ref().filter(|p| {
                matches!(
                    p.scratch,
                    GraphDisposalStep::Applied | GraphDisposalStep::NotApplicable
                )
            }) {
                prior.scratch
            } else if permit.is_none() || shared.contains(thread) {
                GraphDisposalStep::NotApplicable
            } else if let (Some(scratch), Some(DisposalScratchUse::Unused { epoch, lease })) =
                (&scratch, snapshots.get(thread))
            {
                let effect_key = crate::scratch::GraphDisposalKey::try_from(&key).map_err(error)?;
                match crate::ownership::disposal::reset_unused(authority, thread, lease, || {
                    scratch.reset_thread_if_unused(thread, *epoch, lease, &effect_key)
                }) {
                    Ok(Some(_)) => GraphDisposalStep::Applied,
                    Ok(None) | Err(_) => GraphDisposalStep::Unknown,
                }
            } else {
                GraphDisposalStep::Unknown
            };
            let request = GraphThreadProgressWrite {
                key: key.clone(),
                expected_revision: prior.as_ref().map_or(0, |p| p.revision),
                control: control.clone(),
                owners,
                scratch: scratch_step,
            };
            let confirmed = store
                .handle
                .write_recovery_graph_progress(request)
                .or_else(|_| {
                    store
                        .with_database(|db| db.recovery_graph_thread_progress(&key))?
                        .filter(|p| {
                            p.effect == effect && p.owners == owners && p.scratch == scratch_step
                        })
                        .ok_or_else(|| {
                            storage::StorageError::Io("disposal progress unknown".into())
                        })
                })
                .is_ok();
            if !confirmed {
                result.unknown = true;
            }
            if confirmed
                && scratch_step == GraphDisposalStep::Applied
                && let Some(scratch) = &scratch
            {
                scratch.forget_durable_reset(
                    &crate::scratch::GraphDisposalKey::try_from(&key).map_err(error)?,
                );
            }
            result.threads.push(DisposalThreadResult {
                thread_id: thread.clone(),
                owners,
                scratch: scratch_step,
            });
        }
        // Existing deferred trash mechanism only, after owners/scratch locks.
        if let Some(scratch) = &scratch
            && scratch.wake_trash().is_err()
        {
            result.unknown = true;
        }
        let finished = store
            .handle
            .finish_recovery_graph_control(control.clone())
            .or_else(|_| {
                store
                    .with_database(|db| db.recovery_graph_effect(&effect.operation_id))?
                    .filter(|c| c.effect == effect && c.revoked && c.mode == "revoked_idle")
                    .ok_or_else(|| storage::StorageError::Io("disposal completion unknown".into()))
            });
        match finished {
            Ok(c) => result.control = Some(c),
            Err(_) => result.unknown = true,
        };
        Ok(result)
    }

    fn disposal_runtime_users(
        &self,
        expected: &GraphControlCas,
    ) -> Result<BTreeSet<String>, RuntimeError> {
        let roots = self.goal_lock().roots.clone();
        let intents = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let runs = lock_runs(&self.shared.runs);
        let target = expected
            .manifest
            .members
            .iter()
            .map(|m| m.run_id.parse::<RunId>().map_err(error))
            .collect::<Result<BTreeSet<_>, _>>()?;
        let mut result = BTreeSet::new();
        for (root, thread) in &roots {
            if !target.contains(root) {
                result.insert(thread.clone());
            }
        }
        let mut parents = BTreeMap::new();
        let mut live = BTreeSet::new();
        for (id, intent) in intents.iter() {
            parents.insert(*id, intent.parent);
            if intent.execution.is_some() || intent.paused_recovery.is_some() {
                live.insert(*id);
            }
        }
        for (id, admission) in admissions.iter() {
            let snapshot = admission.snapshot();
            parents.insert(*id, snapshot.parent);
            live.insert(*id);
            if let Some(p) = snapshot.ownership {
                result.insert(p.thread_id);
            }
        }
        for (id, run) in runs.iter() {
            parents.insert(*id, run.parent);
            if !matches!(
                *run.phase_rx.borrow(),
                AgentRunPhase::Stopped | AgentRunPhase::Done | AgentRunPhase::Error
            ) {
                live.insert(*id);
                if let Some(p) = &run.config.ownership {
                    result.insert(p.thread_id.clone());
                }
            }
        }
        for id in live {
            // Phase one removed captured paused indexes. A still-live execution
            // of the same ID MUST NOT be excluded.
            let mut current = id;
            let mut seen = BTreeSet::new();
            loop {
                if !seen.insert(current) {
                    return Err(error("cyclic runtime scratch ancestry"));
                }
                if let Some(thread) = roots.get(&current) {
                    result.insert(thread.clone());
                    break;
                }
                match parents.get(&current).copied().flatten() {
                    Some(parent) => current = parent,
                    None => break,
                }
            }
        }
        Ok(result)
    }
}
fn error(reason: impl ToString) -> RuntimeError {
    crate::quiesce::runtime::error(reason)
}
