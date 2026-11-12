use super::{Command, StorageHandle};
use crate::{StorageError, recovery::*};
impl StorageHandle {
    pub fn prepare_recovery_manifest(
        &self,
        record: RecoveryManifestRecord,
    ) -> Result<(), StorageError> {
        self.recovery_write(Write::Prepare(record)).map(|_| ())
    }
    pub fn commit_recovery_manifest(&self, id: String) -> Result<(), StorageError> {
        self.recovery_write(Write::Commit(id)).map(|_| ())
    }
    pub fn claim_recovery_manifest(
        &self,
        request: RecoveryClaimRequest,
    ) -> Result<RecoveryClaim, StorageError> {
        match self.recovery_write(Write::Claim(request))? {
            Written::Claimed(c) => Ok(c),
            _ => unreachable!("claim command reply"),
        }
    }
    /// Read DB revision again immediately before the later activation boundary.
    pub fn validate_recovery_claim(&self, id: String) -> Result<(), StorageError> {
        self.recovery_write(Write::Validate(id)).map(|_| ())
    }
    /// Inspect an existing reservation without a new claim or revision. The
    /// returned opaque evidence is tied to this DB and does not grant authority.
    pub fn validate_prepared_claim(
        &self,
        id: String,
        startup: &crate::RecoveryStartupComplete,
        ownership_root: &std::path::Path,
    ) -> Result<ValidatedPreparedClaim, StorageError> {
        startup.validate(&self.2.db_path, ownership_root)?;
        match self.recovery_write(Write::InspectPrepared(id))? {
            Written::Validated(evidence) => Ok(ValidatedPreparedClaim::from_evidence(
                self.2.db_path.clone(),
                *evidence,
            )),
            _ => unreachable!("prepared evidence command reply"),
        }
    }
    /// The runtime's prepare entry point must hold shared startup evidence for
    /// this writer's DB and its current ownership root.
    pub fn claim_recovery_component(
        &self,
        request: RecoveryClaimRequest,
        startup: &crate::RecoveryStartupComplete,
        ownership_root: &std::path::Path,
    ) -> Result<RecoveryClaim, StorageError> {
        startup.validate(&self.2.db_path, ownership_root)?;
        self.claim_recovery_manifest(request)
    }
    /// FULL-durable all-member activation. No snapshot consumption marker is written.
    pub fn activate_recovery_claim(
        &self,
        request: RecoveryActivationRequest,
    ) -> Result<RecoveryClaim, StorageError> {
        match self.recovery_write(Write::Activate(request))? {
            Written::Claimed(claim) => Ok(claim),
            _ => unreachable!("activation command reply"),
        }
    }
    /// Release only a provably unexecuted claim, without undoing intent revisions.
    pub fn release_recovery_claim(
        &self,
        claim_id: String,
        reason: String,
    ) -> Result<RecoveryClaim, StorageError> {
        self.release_prepared_claim_if_unexecuted(claim_id, reason)
            .map(|outcome| outcome.claim)
    }

    /// The all-member old-incarnation/revision check and release happen inside
    /// the same FULL writer transaction. Explicit Stop is reported separately.
    pub fn release_prepared_claim_if_unexecuted(
        &self,
        claim_id: String,
        reason: String,
    ) -> Result<RecoveryReleaseOutcome, StorageError> {
        match self.recovery_write(Write::Release { claim_id, reason })? {
            Written::Released(outcome) => Ok(outcome),
            _ => unreachable!("release command reply"),
        }
    }
    fn recovery_write(&self, write: Write) -> Result<Written, StorageError> {
        self.request(|reply| Command::Recovery(write, reply))
    }
}
