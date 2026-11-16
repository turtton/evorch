//! Internal planned-restart ownership contract, independent of the restart DB.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{Lease, OwnershipError, ThreadOwner};

/// A hold is data, not adoption authority. Lease expiry is deliberately not part
/// of its identity: heartbeat renewal must not invalidate a hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedHold {
    pub transaction_id: String,
    pub capture_id: String,
    pub thread_id: String,
    pub owner_id: String,
    pub generation: u64,
    pub active_runs: BTreeSet<String>,
    /// Captured resumable runs whose owner checkpoint has ended the live turn.
    pub parked_runs: BTreeSet<String>,
    /// Cold claim observed an abandoned hold. Retained for quarantine/discard.
    #[serde(default)]
    pub orphaned: bool,
}

impl PlannedHold {
    pub fn matches_generation(&self, owner: &ThreadOwner) -> bool {
        self.thread_id == owner.thread_id
            && self.owner_id == owner.lease.owner_id
            && self.generation == owner.lease.generation
    }

    pub(super) fn lease(&self) -> Lease {
        Lease {
            owner_id: self.owner_id.clone(),
            generation: self.generation,
            expires_at: 0,
        }
    }
}

/// Capture's active owner-run sets, keyed by thread. An omitted idle owned
/// thread gets an empty set, but an omitted active thread is an error.
pub type ExpectedActiveRuns = BTreeMap<String, ExpectedThreadRuns>;

/// Capture membership is not the owners.db live-turn inventory.
pub type ExpectedCapturedRuns = BTreeMap<String, CapturedThreadRuns>;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedThreadRuns {
    pub generation: u64,
    pub captured_runs: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedThreadRuns {
    pub generation: u64,
    pub active_runs: BTreeSet<String>,
}

/// Evidence supplied by the adopter, NOT reconstructed from owners.db JSON.
///
/// The implementation must hold verified inherited handoff proof and an Adopted
/// transaction readback, and validate tx/capture, predecessor identity/generation,
/// thread membership and each expected run's incarnation against that readback.
/// Idle owned threads are authorized by the predecessor's full hold inventory,
/// not by fabricating capture members. Errors fail closed before any mutation.
/// This seam keeps the two DBs independent; it is not a cross-DB transaction.
/// It must not call back into this ownership registry while validating.
pub trait AdoptedOwnershipEvidence {
    fn validate_hold(&self, hold: &PlannedHold) -> Result<(), OwnershipError>;
}

impl ThreadOwner {
    pub fn has_planned_hold(&self) -> bool {
        self.handoff_hold
            .as_ref()
            .is_some_and(|hold| !hold.orphaned && hold.matches_generation(self))
    }

    /// Orphan evidence protects scratch even after a cold claim advanced the generation.
    /// It does not block ordinary ownership operations.
    pub fn protects_scratch(&self) -> bool {
        self.has_planned_hold() || self.handoff_hold.as_ref().is_some_and(|hold| hold.orphaned)
    }

    pub(super) fn check_active_runs(
        &self,
        expected: &BTreeSet<String>,
    ) -> Result<(), OwnershipError> {
        if &self.active_runs != expected || self.active_turn != !expected.is_empty() {
            return Err(OwnershipError::PlannedMismatch);
        }
        Ok(())
    }

    pub(super) fn install_hold(
        &mut self,
        tx: &str,
        capture: &str,
        expected: &BTreeSet<String>,
    ) -> Result<(), OwnershipError> {
        self.install_captured_hold(tx, capture, expected, &BTreeSet::new())
    }

    pub(super) fn install_captured_hold(
        &mut self,
        tx: &str,
        capture: &str,
        expected: &BTreeSet<String>,
        parked: &BTreeSet<String>,
    ) -> Result<(), OwnershipError> {
        if !expected.is_disjoint(parked)
            || tx.is_empty()
            || capture.is_empty()
            || self.state == super::OwnerState::Released
        {
            return Err(OwnershipError::PlannedMismatch);
        }
        self.check_active_runs(expected)?;
        let hold = PlannedHold {
            transaction_id: tx.into(),
            capture_id: capture.into(),
            thread_id: self.thread_id.clone(),
            owner_id: self.lease.owner_id.clone(),
            generation: self.lease.generation,
            active_runs: expected.clone(),
            parked_runs: parked.clone(),
            orphaned: false,
        };
        if self.protects_scratch() && self.handoff_hold.as_deref() != Some(&hold) {
            return Err(OwnershipError::PlannedHeld);
        }
        self.state = super::OwnerState::Quiescing;
        self.handoff_hold = Some(Box::new(hold));
        Ok(())
    }

    /// Every FailedClosed path may call this, not only exec syscall failure.
    /// It keeps the generation, active turns and quiescing state intact.
    pub(super) fn clear_hold(&mut self, hold: &PlannedHold) -> Result<(), OwnershipError> {
        self.validate_planned_generation(hold)?;
        if self.handoff_hold.as_deref() == Some(hold) {
            self.handoff_hold = None;
        } else if self.handoff_hold.is_some() {
            return Err(OwnershipError::PlannedMismatch);
        }
        Ok(())
    }

    pub(super) fn validate_planned_generation(
        &self,
        hold: &PlannedHold,
    ) -> Result<(), OwnershipError> {
        if !hold.matches_generation(self) {
            return Err(OwnershipError::PlannedOwnerBlocked);
        }
        Ok(())
    }

    pub(super) fn claim_planned(
        &mut self,
        hold: &PlannedHold,
        successor: &str,
        now_ms: u64,
    ) -> Result<(), OwnershipError> {
        self.validate_planned_generation(hold)?;
        if self.handoff_hold.as_deref() != Some(hold) || self.state != super::OwnerState::Quiescing
        {
            return Err(OwnershipError::PlannedMismatch);
        }
        if !hold.active_runs.is_disjoint(&hold.parked_runs) {
            return Err(OwnershipError::PlannedMismatch);
        }
        self.check_active_runs(&hold.active_runs)?;
        let generation = self
            .lease
            .generation
            .checked_add(1)
            .ok_or(OwnershipError::GenerationExhausted)?;
        self.lease = Lease {
            owner_id: successor.into(),
            generation,
            expires_at: now_ms.saturating_add(self.settings.lease_ms.get()),
        };
        self.active_turn = false;
        self.active_runs.clear();
        self.handoff_hold = None;
        self.planned_claim = Some(Box::new(hold.clone()));
        self.state = super::OwnerState::Running;
        Ok(())
    }
}
