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
            Written::Saved => unreachable!("claim command reply"),
        }
    }
    /// Read DB revision again immediately before the later activation boundary.
    pub fn validate_recovery_claim(&self, id: String) -> Result<(), StorageError> {
        self.recovery_write(Write::Validate(id)).map(|_| ())
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
            Written::Saved => unreachable!("activation command reply"),
        }
    }
    /// Release only an unactivated claim, without rolling back intent revisions.
    pub fn release_recovery_claim(
        &self,
        claim_id: String,
        reason: String,
    ) -> Result<RecoveryClaim, StorageError> {
        match self.recovery_write(Write::Release { claim_id, reason })? {
            Written::Claimed(claim) => Ok(claim),
            Written::Saved => unreachable!("release command reply"),
        }
    }
    fn recovery_write(&self, write: Write) -> Result<Written, StorageError> {
        self.request(|reply| Command::Recovery(write, reply))
    }
}
