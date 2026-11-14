//! Prepared evidence and release share the recovery writer's FULL transaction.
use super::*;
use std::path::{Path, PathBuf};

/// Verified at a writer boundary, not execution authority. A later activation
/// must repeat the fences: neither this value nor a saved recipe grants a permit.
/// There is deliberately no public constructor, Clone or deserializer.
pub struct ValidatedPreparedClaim {
    database: PathBuf,
    evidence: PreparedClaimEvidence,
}

impl std::fmt::Debug for ValidatedPreparedClaim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValidatedPreparedClaim")
            .field("claim_id", &self.evidence.claim.claim_id)
            .finish_non_exhaustive()
    }
}

impl ValidatedPreparedClaim {
    pub(crate) fn from_evidence(database: PathBuf, evidence: PreparedClaimEvidence) -> Self {
        Self { database, evidence }
    }

    pub fn claim(&self) -> &RecoveryClaim {
        &self.evidence.claim
    }

    pub fn manifest(&self) -> &RecoveryManifestRecord {
        &self.evidence.manifest
    }

    pub fn snapshots(&self) -> &[RecoverySnapshotExpectation] {
        &self.evidence.snapshots
    }

    pub fn intents(&self) -> &[crate::resume_intent::ResumeIntentRecord] {
        &self.evidence.intents
    }

    /// A proof issued by another database cannot select an incarnation here.
    pub fn validate_database(&self, database: &Path) -> Result<(), StorageError> {
        let canonical = |path: &Path| {
            std::fs::canonicalize(path).map_err(|_| invalid("recovery database unavailable"))
        };
        if canonical(&self.database)? != canonical(database)? {
            return Err(invalid(
                "prepared recovery proof belongs to another database",
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct PreparedClaimEvidence {
    claim: RecoveryClaim,
    manifest: RecoveryManifestRecord,
    snapshots: Vec<RecoverySnapshotExpectation>,
    intents: Vec<crate::resume_intent::ResumeIntentRecord>,
}

/// Stop is a distinct, positively verified outcome, not a generic CAS conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryReleaseOutcome {
    pub claim: RecoveryClaim,
    pub stopped_members: Vec<String>,
}

pub(super) fn membership(
    conn: &Connection,
    id: &str,
) -> Result<(RecoveryClaim, RecoveryManifestRecord), StorageError> {
    let claim = super::claim(conn, id)?;
    if claim.state != RecoveryClaimState::Prepared {
        return Err(invalid("recovery claim is not prepared"));
    }
    let manifest =
        load(conn, &claim.manifest_id).map_err(|_| invalid("corrupt recovery manifest"))?;
    if !manifest.committed || manifest.superseded || manifest.schema_version != 1 {
        return Err(invalid("recovery manifest is not current"));
    }
    let resume = manifest
        .members
        .iter()
        .filter(|m| m.resume)
        .collect::<Vec<_>>();
    let ids = claim
        .members
        .iter()
        .map(|m| &m.run_id)
        .collect::<BTreeSet<_>>();
    if resume.is_empty() || resume.len() != claim.members.len() || ids.len() != resume.len() {
        return Err(invalid("prepared recovery membership mismatch"));
    }
    for member in resume {
        let claimed = claim
            .members
            .iter()
            .find(|c| c.run_id == member.run_id)
            .ok_or_else(|| invalid("missing prepared recovery member"))?;
        if claimed.old_incarnation != member.incarnation
            || member.intent_revision.checked_add(1) != Some(claimed.reserved_revision)
            || claimed.new_incarnation.is_empty()
            || claimed.new_incarnation == claimed.old_incarnation
        {
            return Err(invalid("prepared recovery member identity mismatch"));
        }
    }
    // Runtime owns the full record schema; malformed JSON is not evidence.
    for member in &manifest.members {
        exact_json(&member.record_json)?;
    }
    Ok((claim, manifest))
}

pub(super) fn inspect(conn: &Connection, id: &str) -> Result<PreparedClaimEvidence, StorageError> {
    validate_claim(conn, id)?;
    let (claim, manifest) = membership(conn, id)?;
    let mut snapshots = Vec::new();
    let mut intents = Vec::new();
    for member in &manifest.members {
        intents.push(
            super::inspection::intent(conn, &member.run_id)?
                .intent
                .ok_or_else(|| invalid("missing prepared recovery intent"))?,
        );
        if let Some(receipt) = &member.snapshot {
            let snapshot = crate::repo::run_context::get(conn, &member.run_id)?
                .ok_or_else(|| invalid("missing prepared recovery snapshot"))?;
            let descriptor = exact_json(&snapshot.config_json)?;
            if descriptor
                .get("admission_reservation_id")
                .is_some_and(|value| !value.is_null())
            {
                return Err(invalid("prepared recovery snapshot already reserved"));
            }
            snapshots.push(RecoverySnapshotExpectation {
                run_id: member.run_id.clone(),
                receipt: receipt.clone(),
                config_json: snapshot.config_json,
            });
        }
    }
    Ok(PreparedClaimEvidence {
        claim,
        manifest,
        snapshots,
        intents,
    })
}

pub(super) fn release(
    conn: &Connection,
    id: &str,
    reason: &str,
) -> Result<RecoveryReleaseOutcome, StorageError> {
    let (claim, manifest) = membership(conn, id)?;
    let mut stopped_members = Vec::new();
    for member in &manifest.members {
        let row: Option<(String, u64, String)> = conn
            .query_row(
                "SELECT incarnation,revision,payload FROM resume_intents WHERE run_id=?1 \
                 AND NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=?1)",
                [&member.run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let (incarnation, revision, payload) =
            row.ok_or_else(|| invalid("missing or tombstoned recovery member"))?;
        let payload = exact_json(&payload)?;
        if incarnation != member.incarnation
            || payload.get("incarnation").and_then(|v| v.as_str()) != Some(&incarnation)
            || payload.get("run_id").and_then(|v| v.as_str()) != Some(&member.run_id)
        {
            return Err(invalid("recovery release incarnation conflict"));
        }
        let reserved = if member.resume {
            claim
                .members
                .iter()
                .find(|m| m.run_id == member.run_id)
                .expect("membership checked")
                .reserved_revision
        } else {
            member.intent_revision
        };
        if member.resume {
            let active = payload.get("active").and_then(|v| v.as_bool());
            let stopped = matches!(
                payload.get("stop_reason").and_then(|v| v.as_str()),
                Some("operator_stopped" | "cancelled")
            );
            if revision > reserved && active == Some(false) && stopped {
                stopped_members.push(member.run_id.clone());
                continue;
            }
            if active != Some(true)
                || !payload
                    .get("stop_reason")
                    .is_some_and(serde_json::Value::is_null)
            {
                return Err(invalid("recovery release lacks unexecuted evidence"));
            }
        }
        if revision != reserved {
            return Err(invalid("recovery release revision conflict"));
        }
    }
    // Snapshot loss/configuration changes may be why we release. Do not require
    // a usable snapshot here, and never alter it or undo a revision/Stop fence.
    let claim = activation::release(conn, id, reason)?;
    Ok(RecoveryReleaseOutcome {
        claim,
        stopped_members,
    })
}
