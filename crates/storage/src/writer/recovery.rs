use super::{Command, StorageHandle};
use crate::{StorageError, recovery::*};
impl StorageHandle {
    /// Quarantine unreadable restart evidence without decoding a partial graph.
    pub fn quarantine_cold_restart(
        &self,
        id: String,
        expected_revision: u64,
    ) -> Result<(), StorageError> {
        self.recovery_write(Write::RestartColdQuarantine {
            id,
            expected_revision,
        })
        .map(|_| ())
    }
    /// Independent of manifest/member decoding and terminal intent persistence.
    pub fn invalidate_restart(
        &self,
        id: String,
        expected_revision: u64,
        reason: String,
    ) -> Result<(), StorageError> {
        self.recovery_write(Write::RestartInvalidation {
            id,
            expected_revision,
            reason,
        })
        .map(|_| ())
    }

    /// FULL-durable revision CAS. On an uncertain acknowledgement read back the
    /// same restart ID; never issue another capture or reopen optimistically.
    pub fn mutate_restart(
        &self,
        id: String,
        expected_revision: Option<u64>,
        mutation: restart::RestartMutation,
    ) -> Result<restart::RestartTransaction, StorageError> {
        match self.recovery_write(Write::Restart {
            id,
            expected_revision,
            mutation,
        })? {
            Written::Restart(tx) => Ok(*tx),
            _ => unreachable!("restart reply"),
        }
    }
    /// Validate process-local shared startup evidence without reserving a claim.
    pub fn validate_recovery_startup(
        &self,
        startup: &crate::RecoveryStartupComplete,
        db_path: &std::path::Path,
        ownership_root: &std::path::Path,
    ) -> Result<(), StorageError> {
        startup.validate(&self.2.db_path, ownership_root)?;
        startup.validate(db_path, ownership_root)
    }
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
    /// The caller must hold current ownership mutation guards and its local
    /// unpublished-control fences. Every payload/revision/claim identity is CAS
    /// checked together; a conflict never counts as a successful Stop.
    pub fn stop_quarantined_recovery_intents(
        &self,
        claim_id: String,
        expected: Vec<(crate::resume_intent::ResumeIntentRecord, u64)>,
        stopped: Vec<crate::resume_intent::ResumeIntentRecord>,
    ) -> Result<(), StorageError> {
        self.recovery_write(Write::StopQuarantined {
            claim_id,
            expected,
            stopped,
        })
        .map(|_| ())
    }
    fn recovery_write(&self, write: Write) -> Result<Written, StorageError> {
        self.request(|reply| Command::Recovery(write, reply))
    }
}
