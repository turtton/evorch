//! Diagnostic readback is not an incarnation capability or permission to replay.
use super::*;
use crate::resume_intent::ResumeIntentRecord;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryIntentObservation {
    pub run_id: String,
    /// Missing and tombstoned rows are both unavailable for recovery.
    pub intent: Option<ResumeIntentRecord>,
    pub revision: Option<u64>,
}

/// Claim, manifest and all current member intents from one SQLite read snapshot.
/// Contradictions are preserved, not interpreted as successful activation/Stop.
#[derive(Clone, PartialEq, Eq)]
pub struct RecoveryClaimSnapshot {
    pub claim: RecoveryClaim,
    pub manifest: RecoveryManifestRecord,
    pub intents: Vec<RecoveryIntentObservation>,
}

impl std::fmt::Debug for RecoveryClaimSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryClaimSnapshot")
            .field("claim_id", &self.claim.claim_id)
            .field("manifest_id", &self.manifest.manifest_id)
            .field("member_count", &self.intents.len())
            .finish_non_exhaustive()
    }
}

impl Database {
    /// ACK-loss resolution must not combine a pre-commit claim with post-commit
    /// intents. A read error is indeterminate, never an authorization to release.
    pub fn recovery_claim_snapshot(&self, id: &str) -> Result<RecoveryClaimSnapshot, StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        let claim = super::claim(&tx, id)?;
        let manifest =
            load(&tx, &claim.manifest_id).map_err(|_| invalid("corrupt recovery manifest"))?;
        // Include unexpected claim members as well: they cannot disappear from
        // diagnostics just because they are absent from a damaged manifest.
        let ids = manifest
            .members
            .iter()
            .map(|m| m.run_id.as_str())
            .chain(claim.members.iter().map(|m| m.run_id.as_str()))
            .collect::<BTreeSet<_>>();
        let intents = ids
            .into_iter()
            .map(|id| intent(&tx, id))
            .collect::<Result<Vec<_>, _>>()?;
        tx.commit()?;
        Ok(RecoveryClaimSnapshot {
            claim,
            manifest,
            intents,
        })
    }
}

pub(super) fn intent(
    conn: &Connection,
    run_id: &str,
) -> Result<RecoveryIntentObservation, StorageError> {
    let current = conn
        .query_row(
            "SELECT incarnation,payload,revision FROM resume_intents WHERE run_id=?1 \
             AND NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=?1)",
            [run_id],
            |row| {
                Ok((
                    ResumeIntentRecord {
                        run_id: run_id.into(),
                        incarnation: row.get(0)?,
                        payload: row.get(1)?,
                    },
                    row.get::<_, u64>(2)?,
                ))
            },
        )
        .optional()?;
    let (intent, revision) = match current {
        Some((intent, revision)) => (Some(intent), Some(revision)),
        None => (None, None),
    };
    Ok(RecoveryIntentObservation {
        run_id: run_id.into(),
        intent,
        revision,
    })
}
