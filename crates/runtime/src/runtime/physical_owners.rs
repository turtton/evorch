//! Physical-bootstrap permit retention. Cold records cannot reopen this window.
use super::*;
use crate::ownership::{OwnerHost, OwnerPermit, OwnershipError, PlannedHold, RegistryError};
use std::collections::{BTreeMap, BTreeSet};
use storage::recovery::graph_control::GraphControlCas;

#[derive(Default)]
pub(super) struct RetainedPhysicalOwners {
    attempted: bool,
    sealed: Option<PhysicalOwnerSet>,
}
struct PhysicalOwnerSet {
    restart_id: String,
    capture_id: String,
    permits: BTreeMap<String, OwnerPermit>,
}
pub struct PhysicalClaimReport {
    pub blocked_threads: Vec<String>,
}
/// Limited cleanup authority, never execution authority.
///
/// ```no_run
/// fn accept(_: &runtime::DisposalAuthority) {}
/// ```
/// ```compile_fail
/// let authority = runtime::DisposalAuthority {};
/// ```
/// ```compile_fail
/// let authority = <runtime::DisposalAuthority as Default>::default();
/// ```
/// ```compile_fail
/// fn forge(permit: runtime::ownership::OwnerPermit) -> runtime::DisposalAuthority {
///     permit.into()
/// }
/// ```
/// ```compile_fail
/// fn execution(authority: runtime::DisposalAuthority) -> runtime::RunConfig {
///     authority.into()
/// }
/// ```
pub struct DisposalAuthority {
    runtime: Weak<Shared>,
    expected: GraphControlCas,
    registry_path: PathBuf,
    permits: BTreeMap<String, Option<OwnerPermit>>,
}
impl DisposalAuthority {
    pub(crate) fn runtime(&self) -> &Weak<Shared> {
        &self.runtime
    }
    pub(crate) fn expected(&self) -> &GraphControlCas {
        &self.expected
    }
    pub(crate) fn registry_path(&self) -> &std::path::Path {
        &self.registry_path
    }
    pub(crate) fn permits(&self) -> &BTreeMap<String, Option<OwnerPermit>> {
        &self.permits
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedThreadCas {
    pub thread_id: String,
    pub owner_identity: Option<String>,
    pub owner_generation: Option<u64>,
}
impl AgentRuntime {
    /// The sole retention entrance accepts concrete physical evidence. The
    /// progress callback runs immediately after the first successful claim.
    pub fn claim_physical_bootstrap_owners(
        &self,
        owner: &OwnerHost,
        evidence: RestartOwnershipEvidence,
        first_claim: impl FnOnce(),
    ) -> Result<PhysicalClaimReport, RegistryError> {
        if !Arc::ptr_eq(&self.shared, &evidence.runtime.shared) {
            return Err(OwnershipError::PlannedMismatch.into());
        }
        let tx = self
            .read_adopted_restart(false)
            .map_err(|_| OwnershipError::PlannedMismatch)?;
        {
            let mut retained = self
                .shared
                .retained_physical_owners
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if retained.attempted {
                return Err(OwnershipError::PlannedMismatch.into());
            }
            retained.attempted = true;
        }
        let mut first_claim = Some(first_claim);
        let mut permits = BTreeMap::new();
        let mut blocked_threads = Vec::new();
        for progress in &tx.owner_progress {
            let hold = PlannedHold {
                transaction_id: tx.metadata.restart_id.clone(),
                capture_id: tx.metadata.capture_id.clone(),
                thread_id: progress.thread_id.clone(),
                owner_id: progress.old_owner_id.clone(),
                generation: progress.old_generation,
                active_runs: progress.active_runs.clone(),
                parked_runs: progress.parked_runs.clone(),
                orphaned: false,
            };
            match owner.claim_planned(&hold, &evidence) {
                Ok(permit) => {
                    permits.insert(hold.thread_id, permit);
                    if let Some(first) = first_claim.take() {
                        first();
                    }
                }
                Err(RegistryError::Ownership(OwnershipError::PlannedOwnerBlocked)) => {
                    blocked_threads.push(hold.thread_id)
                }
                Err(error) => return Err(error),
            }
        }
        self.shared
            .retained_physical_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sealed = Some(PhysicalOwnerSet {
            restart_id: tx.metadata.restart_id,
            capture_id: tx.metadata.capture_id,
            permits,
        });
        // Seal has completed; read every graph before the first automatic attempt.
        self.rebuild_graph_observation()
            .map_err(|_| OwnershipError::PlannedMismatch)?;
        Ok(PhysicalClaimReport { blocked_threads })
    }
    /// Restricts the sealed set; neither current DB rows nor caller permits can
    /// add a member. Absent expectations are checked under the same read guard.
    pub fn disposal_authority(
        &self,
        expected: GraphControlCas,
        owners: Vec<OwnedThreadCas>,
    ) -> Result<DisposalAuthority, RuntimeError> {
        if owners.is_empty()
            || owners.len() > 64
            || owners.windows(2).any(|w| w[0].thread_id >= w[1].thread_id)
        {
            return Err(error("invalid disposal owner set"));
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let canonical = store
            .with_database(|db| {
                db.recovery_graph_cas(&expected.restart_id, &expected.manifest.manifest_id)
            })
            .map_err(error)?;
        if canonical != expected
            || !matches!(
                expected.phase,
                storage::recovery::restart::RestartPhase::Adopted
                    | storage::recovery::restart::RestartPhase::Completed
                    | storage::recovery::restart::RestartPhase::Aborted
                    | storage::recovery::restart::RestartPhase::Invalidated
                    | storage::recovery::restart::RestartPhase::Superseded
            )
        {
            return Err(error("disposal graph CAS conflict"));
        }
        let membership = expected
            .membership
            .iter()
            .filter_map(|m| m.thread_id.clone())
            .collect::<BTreeSet<_>>();
        if membership != owners.iter().map(|o| o.thread_id.clone()).collect() {
            return Err(error("incomplete disposal owner set"));
        }
        let retained = self
            .shared
            .retained_physical_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let set = retained
            .sealed
            .as_ref()
            .ok_or_else(|| error("physical owner retention required"))?;
        if set.restart_id != expected.restart_id || set.capture_id != expected.capture_id {
            return Err(error("foreign disposal graph"));
        }
        let mut permits = BTreeMap::new();
        let mut registry_path = None;
        for owner in owners {
            match (&owner.owner_identity, owner.owner_generation) {
                (Some(identity), Some(generation)) => {
                    let permit = set
                        .permits
                        .get(&owner.thread_id)
                        .filter(|p| {
                            &p.lease.owner_id == identity && p.lease.generation == generation
                        })
                        .ok_or_else(|| error("owner not physically retained"))?;
                    if registry_path
                        .as_ref()
                        .is_some_and(|p| p != &permit.registry_path)
                    {
                        return Err(error("mixed owner registries"));
                    }
                    registry_path = Some(permit.registry_path.clone());
                    permits.insert(owner.thread_id, Some(permit.clone()));
                }
                (None, None) => {
                    permits.insert(owner.thread_id, None);
                }
                _ => return Err(error("partial owner expectation")),
            }
        }
        let authority = DisposalAuthority {
            runtime: Arc::downgrade(&self.shared),
            expected,
            registry_path: registry_path.ok_or_else(|| error("no retained graph owner"))?,
            permits,
        };
        let _guard = crate::ownership::disposal_support::guard(&authority).map_err(error)?;
        Ok(authority)
    }
}
fn error(reason: impl ToString) -> RuntimeError {
    crate::quiesce::runtime::error(reason)
}

/// Read-only classification of the entire physical graph, never an operation binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnedGraphOwnershipState {
    LiveSelf,
    MixedSelfAndAbsent,
    Absent,
    Foreign,
    GenerationChanged,
    Unavailable,
    Cold,
}
/// Bounded diagnostic output, including graphs too large to operate on.
/// References are domain-separated hashes, not thread IDs or permits.
/// There is deliberately no conversion into an operation's complete CAS set.
///
/// ```no_run
/// fn diagnostic(rt: &runtime::AgentRuntime,
///     cas: &storage::recovery::graph_control::GraphControlCas) {
///     let view = rt.owned_graph_ownership_diagnostic(cas).unwrap();
///     assert!(view.references().len() <= 64);
/// }
/// ```
///
/// ```compile_fail
/// fn not_a_binding(rt: &runtime::AgentRuntime,
///     cas: storage::recovery::graph_control::GraphControlCas,
///     diagnostic: runtime::OwnedGraphOwnershipDiagnostic) {
///     rt.disposal_authority(cas, diagnostic);
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedGraphOwnershipDiagnostic {
    state: OwnedGraphOwnershipState,
    verified_adoption: bool,
    retained_owner_count: u64,
    owner_count: u64,
    references: Vec<String>,
}
impl OwnedGraphOwnershipDiagnostic {
    pub fn state(&self) -> OwnedGraphOwnershipState {
        self.state
    }
    pub fn verified_adoption(&self) -> bool {
        self.verified_adoption
    }
    pub fn retained_owner_count(&self) -> u64 {
        self.retained_owner_count
    }
    pub fn owner_count(&self) -> u64 {
        self.owner_count
    }
    pub fn references(&self) -> &[String] {
        &self.references
    }
    pub fn truncated(&self) -> bool {
        self.owner_count > self.references.len() as u64
    }
}
impl AgentRuntime {
    /// Inspect every member under one read-only owners snapshot. Unlike the
    /// authority constructor, this does not truncate the ownership check at 64.
    /// Neither the sealed set nor a graph authority is returned or reconstructed.
    pub fn owned_graph_ownership_diagnostic(
        &self,
        expected: &GraphControlCas,
    ) -> Result<OwnedGraphOwnershipDiagnostic, RuntimeError> {
        // A diagnostic may observe owner validity loss, but cannot settle an
        // existing effect's Unknown ticket. External generations only advance.
        let observation = self
            .begin_graph_change(&expected.manifest.manifest_id, false)
            .ok()
            .flatten();
        let result = self.owned_graph_ownership_readback(expected);
        if let Some(observation) = observation {
            if result.is_ok() {
                let _ = observation.confirm();
            } else {
                observation.mark_unavailable();
            }
        }
        result
    }
    /// Collector readback only; no recursive observation hook. This read grants
    /// no permits and inspects the sealed inventory, never current DB claims.
    pub fn owned_graph_ownership_readback(
        &self,
        expected: &GraphControlCas,
    ) -> Result<OwnedGraphOwnershipDiagnostic, RuntimeError> {
        use sha2::{Digest, Sha256};
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let current = store
            .with_database(|db| {
                db.recovery_graph_cas(&expected.restart_id, &expected.manifest.manifest_id)
            })
            .map_err(error)?;
        if current != *expected {
            return Err(error("diagnostic graph CAS conflict"));
        }
        let threads: BTreeSet<_> = expected
            .membership
            .iter()
            .filter_map(|member| member.thread_id.clone())
            .collect();
        let references = threads
            .iter()
            .take(64)
            .map(|thread| {
                let mut hash = Sha256::new();
                for part in [
                    "physical-owner-diagnostic",
                    &expected.restart_id,
                    &expected.capture_id,
                    &expected.manifest.manifest_id,
                    thread,
                ] {
                    hash.update((part.len() as u64).to_be_bytes());
                    hash.update(part.as_bytes());
                }
                format!("{:x}", hash.finalize())
            })
            .collect();
        let mut result = OwnedGraphOwnershipDiagnostic {
            state: OwnedGraphOwnershipState::Cold,
            verified_adoption: false,
            retained_owner_count: 0,
            owner_count: threads.len() as u64,
            references,
        };
        let retained = self
            .shared
            .retained_physical_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(set) = &retained.sealed else {
            return Ok(result);
        };
        if set.restart_id != expected.restart_id || set.capture_id != expected.capture_id {
            return Ok(result);
        }
        result.verified_adoption = true;
        let Some(first) = set.permits.values().next() else {
            result.state = OwnedGraphOwnershipState::Absent;
            return Ok(result);
        };
        let diagnosis = crate::ownership::disposal_support::diagnose(
            &first.registry_path,
            expected,
            &set.permits,
            &threads,
        )
        .map_err(error)?;
        result.retained_owner_count = diagnosis.retained as u64;
        result.state = if diagnosis.foreign {
            OwnedGraphOwnershipState::Foreign
        } else if diagnosis.changed {
            OwnedGraphOwnershipState::GenerationChanged
        } else if diagnosis.unavailable {
            OwnedGraphOwnershipState::Unavailable
        } else if diagnosis.absent == threads.len() {
            OwnedGraphOwnershipState::Absent
        } else if diagnosis.absent > 0 {
            OwnedGraphOwnershipState::MixedSelfAndAbsent
        } else {
            OwnedGraphOwnershipState::LiveSelf
        };
        Ok(result)
    }
}
