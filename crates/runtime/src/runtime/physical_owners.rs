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
    pub(crate) runtime: Weak<Shared>,
    pub(crate) expected: GraphControlCas,
    pub(crate) registry_path: PathBuf,
    pub(crate) permits: BTreeMap<String, Option<OwnerPermit>>,
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
