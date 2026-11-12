//! Activation is one FULL transaction in the recovery writer, not an admission.
use super::*;
use crate::resume_intent::ResumeIntentRecord;

/// A receipt alone does not cover the restore descriptor. Compare both exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverySnapshotExpectation {
    pub run_id: String,
    pub receipt: RunContextReceipt,
    pub config_json: String,
}
#[derive(Debug, Clone)]
pub struct RecoveryActivationRequest {
    pub claim_id: String,
    /// Exactly the resume members, never terminal observation-only endpoints.
    pub intents: Vec<ResumeIntentRecord>,
    /// Exactly the manifest's snapshot-bearing members, including terminal ones.
    pub snapshots: Vec<RecoverySnapshotExpectation>,
}
/// Read-only diagnosis after an uncertain ACK. Never authorizes retry or release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryActivationObservation {
    Activated,
    NotActivated,
    Indeterminate,
}
impl Database {
    /// Intent identities/revisions are primary evidence even if the claim row is
    /// missing. A later checkpoint in the new incarnation still means execution
    /// may have begun; it must not be mistaken for an unactivated claim.
    pub fn observe_recovery_activation(
        &self,
        expected: &RecoveryClaim,
    ) -> Result<RecoveryActivationObservation, StorageError> {
        use RecoveryActivationObservation::*;
        if expected.members.is_empty() {
            return Ok(Indeterminate);
        }
        let tx = self.conn.unchecked_transaction()?;
        let mut old = 0;
        let mut new = 0;
        let mut ids = BTreeSet::new();
        for m in &expected.members {
            if !ids.insert(&m.run_id) {
                return Ok(Indeterminate);
            }
            let row: Option<(String, u64)> = tx
                .query_row(
                    "SELECT incarnation,revision FROM resume_intents WHERE run_id=?1 \
                 AND NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=?1)",
                    [&m.run_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            match row {
                Some((inc, rev)) if inc == m.old_incarnation && rev == m.reserved_revision => {
                    old += 1
                }
                Some((inc, rev)) if inc == m.new_incarnation && rev > m.reserved_revision => {
                    new += 1
                }
                _ => return Ok(Indeterminate),
            }
        }
        Ok(if new == expected.members.len() {
            Activated
        } else if old == expected.members.len() {
            NotActivated
        } else {
            Indeterminate
        })
    }
}

pub(super) fn activate(
    conn: &Connection,
    request: &RecoveryActivationRequest,
) -> Result<RecoveryClaim, StorageError> {
    validate_claim(conn, &request.claim_id)?;
    let claim = claim(conn, &request.claim_id)?;
    let manifest =
        load(conn, &claim.manifest_id).map_err(|_| invalid("corrupt recovery manifest"))?;
    let resume: BTreeSet<_> = manifest
        .members
        .iter()
        .filter(|m| m.resume)
        .map(|m| m.run_id.as_str())
        .collect();
    let claimed: BTreeSet<_> = claim.members.iter().map(|m| m.run_id.as_str()).collect();
    let supplied: BTreeSet<_> = request.intents.iter().map(|m| m.run_id.as_str()).collect();
    if resume.is_empty()
        || resume != claimed
        || resume != supplied
        || claim.members.len() != resume.len()
        || request.intents.len() != resume.len()
    {
        return Err(invalid("activation membership mismatch"));
    }
    let snapshots: BTreeSet<_> = manifest
        .members
        .iter()
        .filter(|m| m.snapshot.is_some())
        .map(|m| m.run_id.as_str())
        .collect();
    let expected: BTreeSet<_> = request
        .snapshots
        .iter()
        .map(|m| m.run_id.as_str())
        .collect();
    if snapshots != expected || expected.len() != request.snapshots.len() {
        return Err(invalid("activation snapshot membership mismatch"));
    }
    for expectation in &request.snapshots {
        let member = manifest
            .members
            .iter()
            .find(|m| m.run_id == expectation.run_id)
            .ok_or_else(|| invalid("missing activation snapshot member"))?;
        let snapshot = crate::repo::run_context::get(conn, &expectation.run_id)?
            .ok_or_else(|| invalid("missing activation snapshot"))?;
        if member.snapshot.as_ref() != Some(&expectation.receipt)
            || !expectation.receipt.matches(&snapshot)
            || snapshot.config_json != expectation.config_json
        {
            return Err(invalid("activation snapshot expectation conflict"));
        }
        let descriptor = exact_json(&snapshot.config_json)?;
        if descriptor
            .get("non_restorable_reason")
            .and_then(|v| v.as_str())
            == Some("snapshot_consumed")
            || descriptor
                .get("admission_reservation_id")
                .is_some_and(|v| !v.is_null())
        {
            return Err(invalid("activation snapshot already reserved"));
        }
    }
    // The only durable payload change here is the incarnation: provider prefix,
    // continuation and identities (run/parent/role) remain the captured intent.
    for intent in &request.intents {
        let m = claim
            .members
            .iter()
            .find(|m| m.run_id == intent.run_id)
            .ok_or_else(|| invalid("missing activation member"))?;
        if intent.incarnation != m.new_incarnation
            || m.new_incarnation == m.old_incarnation
            || m.new_incarnation.is_empty()
        {
            return Err(invalid("activation incarnation mismatch"));
        }
        let payload: String = conn.query_row(
            "SELECT payload FROM resume_intents WHERE run_id=?1",
            [&m.run_id],
            |r| r.get(0),
        )?;
        let mut expected = exact_json(&payload)?;
        let member = manifest
            .members
            .iter()
            .find(|member| member.run_id == intent.run_id)
            .ok_or_else(|| invalid("missing activation manifest member"))?;
        let identity = exact_json(&member.record_json)?;
        if identity.get("run_id").and_then(|v| v.as_str()) != Some(intent.run_id.as_str())
            || identity.get("old_incarnation").and_then(|v| v.as_str())
                != Some(m.old_incarnation.as_str())
            || identity.get("role") != expected.get("role")
            || identity.get("parent") != expected.get("parent_run_id")
        {
            return Err(invalid("activation manifest identity mismatch"));
        }
        expected["incarnation"] = serde_json::Value::String(m.new_incarnation.clone());
        if exact_json(&intent.payload)? != expected {
            return Err(invalid("activation changed captured intent"));
        }
    }
    for intent in &request.intents {
        let m = claim
            .members
            .iter()
            .find(|m| m.run_id == intent.run_id)
            .ok_or_else(|| invalid("missing activation member"))?;
        let changed = conn.execute(
            "UPDATE resume_intents SET incarnation=?2,payload=?3 WHERE run_id=?1 AND incarnation=?4 AND revision=?5 \
             AND NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=?1)",
            params![m.run_id, intent.incarnation, intent.payload, m.old_incarnation, m.reserved_revision],
        )?;
        if changed != 1 {
            return Err(invalid("activation intent CAS conflict"));
        }
    }
    let changed = conn.execute(
        "UPDATE recovery_claims SET state='activated' WHERE claim_id=?1 AND state='prepared'",
        [&claim.claim_id],
    )?;
    if changed != 1 {
        return Err(invalid("activation claim CAS conflict"));
    }
    let changed = conn.execute("UPDATE recovery_manifests SET superseded=1 WHERE manifest_id=?1 AND committed=1 AND superseded=0", [&claim.manifest_id])?;
    if changed != 1 {
        return Err(invalid("activation manifest CAS conflict"));
    }
    super::claim(conn, &claim.claim_id)
}

pub(super) fn release(
    conn: &Connection,
    id: &str,
    reason: &str,
) -> Result<RecoveryClaim, StorageError> {
    // Stop may have advanced the revision. Release never undoes that fence.
    if reason.is_empty() {
        return Err(invalid("empty recovery release reason"));
    }
    exact_json(&serde_json::to_string(reason).map_err(|_| invalid("invalid release reason"))?)?;
    let changed = conn.execute("UPDATE recovery_claims SET state='released',release_reason=?2 WHERE claim_id=?1 AND state='prepared'", params![id, reason])?;
    if changed != 1 {
        return Err(invalid("recovery claim is not prepared"));
    }
    super::claim(conn, id)
}
