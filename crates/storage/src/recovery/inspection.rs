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
    /// Raw relational identities owned by graph recovery, independent of payload
    /// decoding. Orphan intents without a manifest are deliberately NOT included.
    /// An unreadable identity fails closed instead of silently dropping a member.
    pub fn recovery_owned_run_ids(
        &self,
        quarantined_manifests: &BTreeSet<String>,
    ) -> Result<BTreeSet<String>, StorageError> {
        let quarantined = serde_json::to_string(quarantined_manifests)
            .map_err(|error| StorageError::Serialization(error.to_string()))?;
        let tx = self.conn.unchecked_transaction()?;
        let incomplete: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM recovery_manifests g \
             WHERE ((g.committed=1 AND g.superseded=0) OR g.manifest_id IN (SELECT value FROM json_each(?1))) \
             AND (g.member_count<1 OR g.member_count!=(SELECT count(*) FROM recovery_members m WHERE m.manifest_id=g.manifest_id))) \
             OR EXISTS(SELECT 1 FROM recovery_claims c WHERE c.state!='released' AND \
             (NOT EXISTS(SELECT 1 FROM recovery_manifests g WHERE g.manifest_id=c.manifest_id) \
             OR (SELECT count(*) FROM recovery_claim_members m WHERE m.claim_id=c.claim_id)=0 \
             OR (SELECT count(*) FROM recovery_claim_members m WHERE m.claim_id=c.claim_id) \
                 !=(SELECT count(*) FROM recovery_members m WHERE m.manifest_id=c.manifest_id AND m.resume=1)))",
            [&quarantined], |row| row.get(0),
        )?;
        if incomplete {
            return Err(invalid("graph ownership identities unavailable"));
        }
        let mut q = tx.prepare(
            "SELECT m.run_id FROM recovery_members m JOIN recovery_manifests g USING(manifest_id) \
             WHERE (g.committed=1 AND g.superseded=0) \
                OR g.manifest_id IN (SELECT value FROM json_each(?1)) \
             UNION SELECT m.run_id FROM recovery_claim_members m JOIN recovery_claims c USING(claim_id) \
             WHERE c.state!='released' ORDER BY 1",
        )?;
        let ids = q
            .query_map([quarantined], |row| row.get::<_, String>(0))?
            .collect::<Result<BTreeSet<_>, _>>()?;
        drop(q);
        tx.commit()?;
        Ok(ids)
    }

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

/// Called only inside the recovery writer's FULL transaction. No incarnation is
/// learned as authority from the current row: it must be the claim's new one.
pub(super) fn stop_quarantined(
    conn: &Connection,
    claim_id: &str,
    expected: &[(ResumeIntentRecord, u64)],
    stopped: &[ResumeIntentRecord],
) -> Result<(), StorageError> {
    let claim = super::claim(conn, claim_id)?;
    let ids = expected
        .iter()
        .map(|(r, _)| r.run_id.as_str())
        .collect::<BTreeSet<_>>();
    let claimed = claim
        .members
        .iter()
        .map(|m| m.run_id.as_str())
        .collect::<BTreeSet<_>>();
    let supplied = stopped
        .iter()
        .map(|r| r.run_id.as_str())
        .collect::<BTreeSet<_>>();
    if ids.is_empty()
        || ids != claimed
        || ids != supplied
        || ids.len() != expected.len()
        || ids.len() != stopped.len()
    {
        return Err(invalid("quarantine Stop membership mismatch"));
    }
    for (before, revision) in expected {
        let member = claim
            .members
            .iter()
            .find(|m| m.run_id == before.run_id)
            .ok_or_else(|| invalid("quarantine Stop member missing"))?;
        let after = stopped
            .iter()
            .find(|r| r.run_id == before.run_id)
            .ok_or_else(|| invalid("quarantine Stop payload missing"))?;
        let current = intent(conn, &before.run_id)?;
        if before.incarnation != member.new_incarnation
            || after.incarnation != before.incarnation
            || current.intent.as_ref() != Some(before)
            || current.revision != Some(*revision)
        {
            return Err(invalid(
                "quarantine Stop identity/revision/payload conflict",
            ));
        }
        let mut payload = exact_json(&before.payload)?;
        if payload.get("run_id").and_then(serde_json::Value::as_str) != Some(before.run_id.as_str())
            || payload
                .get("incarnation")
                .and_then(serde_json::Value::as_str)
                != Some(before.incarnation.as_str())
        {
            return Err(invalid("quarantine Stop payload identity mismatch"));
        }
        let requested = exact_json(&after.payload)?;
        let reason = requested
            .get("stop_reason")
            .and_then(serde_json::Value::as_str);
        if !matches!(reason, Some("operator_stopped" | "cancelled")) {
            return Err(invalid("quarantine Stop requires explicit Stop/Cancel"));
        }
        let fields = payload
            .as_object_mut()
            .ok_or_else(|| invalid("invalid quarantine Stop payload"))?;
        fields.insert("active".into(), false.into());
        fields.insert("resume_requested".into(), false.into());
        fields.insert("stop_reason".into(), reason.unwrap().into());
        if payload != requested {
            return Err(invalid("quarantine Stop changes unrelated intent fields"));
        }
        crate::resume_intent::save_with_revision(
            conn,
            after,
            Some(&before.incarnation),
            Some(*revision),
        )?;
    }
    Ok(())
}
