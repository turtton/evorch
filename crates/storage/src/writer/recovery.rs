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
    fn recovery_write(&self, write: Write) -> Result<Written, StorageError> {
        self.request(|reply| Command::Recovery(write, reply))
    }
}
