//! Component-scoped graph receipts and durable, revision-fenced claims (PR 06a).
//! Payloads are runtime-owned. No method here starts work or grants authority.
use crate::{Database, RunContextReceipt, StorageError};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryEdge {
    pub from: String,
    pub to: String,
    pub kind: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryMemberRecord {
    pub run_id: String,
    pub incarnation: String,
    pub intent_revision: u64,
    pub snapshot: Option<RunContextReceipt>,
    pub resume: bool,
    pub record_json: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryManifestRecord {
    pub manifest_id: String,
    pub schema_version: u32,
    pub committed: bool,
    pub superseded: bool,
    pub edges: Vec<RecoveryEdge>,
    pub members: Vec<RecoveryMemberRecord>,
}
/// Diagnostic codes deliberately exclude SQLite errors and runtime payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryProblem {
    CorruptManifest,
    CorruptMember,
    MissingMember,
    MissingEdge,
}
#[derive(Debug)]
pub struct RecoveryLoad {
    pub manifest_id: String,
    pub record: Result<RecoveryManifestRecord, RecoveryProblem>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RecoveryClaimState {
    Prepared,
    Activated,
    Released { reason: String },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryClaimMember {
    pub run_id: String,
    pub old_incarnation: String,
    pub reserved_revision: u64,
    pub new_incarnation: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryClaim {
    pub claim_id: String,
    pub manifest_id: String,
    pub state: RecoveryClaimState,
    pub members: Vec<RecoveryClaimMember>,
}
#[derive(Debug, Clone)]
pub struct RecoveryClaimRequest {
    pub claim_id: String,
    pub manifest_id: String,
    /// New incarnation identities only. Claims leave the old intent unchanged.
    pub incarnations: Vec<(String, String)>,
}
#[derive(Debug)]
pub(crate) enum Write {
    Prepare(RecoveryManifestRecord),
    Commit(String),
    Claim(RecoveryClaimRequest),
    Validate(String),
}
#[derive(Debug)]
pub(crate) enum Written {
    Saved,
    Claimed(RecoveryClaim),
}

fn invalid(reason: &str) -> StorageError {
    StorageError::Serialization(reason.into())
}
fn exact_json(payload: &str) -> Result<serde_json::Value, StorageError> {
    let mut value = serde_json::from_str(payload).map_err(|_| invalid("invalid recovery JSON"))?;
    if secret_guard::SecretRedactor::from_env().redact_json(&mut value) != 0 {
        return Err(invalid("recovery evidence contains credentials"));
    }
    Ok(value)
}

pub type RecoveryClaimLoad = (String, Result<RecoveryClaim, RecoveryProblem>);
pub type RecoveryIntentLoad = (
    Option<String>,
    Result<crate::resume_intent::ResumeIntentRecord, StorageError>,
);
impl Database {
    /// A bad payload/attribute is isolated to its manifest, not the entire scan.
    pub fn recovery_manifests(&self) -> Result<Vec<RecoveryLoad>, StorageError> {
        load_all(&self.conn)
    }
    /// Decode each claim independently so one damaged component cannot hide its peers.
    pub fn recovery_claim_loads(&self) -> Result<Vec<RecoveryClaimLoad>, StorageError> {
        let mut q = self
            .conn
            .prepare("SELECT claim_id,manifest_id FROM recovery_claims ORDER BY claim_id")?;
        let mut rows = q.query([])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            let manifest = row
                .get::<_, String>(1)
                .unwrap_or_else(|_| "unreadable".into());
            let record = row
                .get::<_, String>(0)
                .map_err(|_| RecoveryProblem::CorruptManifest)
                .and_then(|id| {
                    claim(&self.conn, &id).map_err(|_| RecoveryProblem::CorruptManifest)
                });
            result.push((manifest, record));
        }
        Ok(result)
    }
    /// Recovery scanning keeps row decoding failures attached to their identity.
    pub fn recovery_intent_loads(&self) -> Result<Vec<RecoveryIntentLoad>, StorageError> {
        let mut q=self.conn.prepare("SELECT run_id,incarnation,payload FROM resume_intents WHERE NOT EXISTS(SELECT 1 FROM run_tombstones t WHERE t.run_id=resume_intents.run_id) ORDER BY run_id")?;
        let mut rows = q.query([])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            let id = row.get::<_, String>(0).ok();
            let record = (|| {
                Ok(crate::resume_intent::ResumeIntentRecord {
                    run_id: row.get(0)?,
                    incarnation: row.get(1)?,
                    payload: row.get(2)?,
                })
            })();
            result.push((id, record));
        }
        Ok(result)
    }
    pub fn recovery_claims(&self) -> Result<Vec<RecoveryClaim>, StorageError> {
        let mut q = self
            .conn
            .prepare("SELECT claim_id FROM recovery_claims ORDER BY claim_id")?;
        let ids = q
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter().map(|id| claim(&self.conn, &id)).collect()
    }
    pub fn resume_intent_revision(&self, run: &str) -> Result<Option<u64>, StorageError> {
        Ok(self.conn.query_row("SELECT revision FROM resume_intents WHERE run_id=?1 AND NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=?1)",[run],|r|r.get(0)).optional()?)
    }
}
fn load_all(conn: &Connection) -> Result<Vec<RecoveryLoad>, StorageError> {
    let mut q = conn.prepare("SELECT manifest_id FROM recovery_manifests ORDER BY manifest_id")?;
    let mut rows = q.query([])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        match row.get::<_, String>(0) {
            Ok(id) => result.push(RecoveryLoad {
                record: load(conn, &id),
                manifest_id: id,
            }),
            Err(_) => result.push(RecoveryLoad {
                record: Err(RecoveryProblem::CorruptManifest),
                manifest_id: "unreadable".into(),
            }),
        }
    }
    Ok(result)
}
fn load(conn: &Connection, id: &str) -> Result<RecoveryManifestRecord, RecoveryProblem> {
    use RecoveryProblem::*;
    let (schema_version,committed,superseded,count,edge_count,edges):(u32,bool,bool,usize,usize,String)=conn.query_row(
        "SELECT schema_version,committed,superseded,member_count,edge_count,edges_json FROM recovery_manifests WHERE manifest_id=?1",[id],
        |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).map_err(|_|CorruptManifest)?;
    let edges: Vec<RecoveryEdge> = serde_json::from_str(&edges).map_err(|_| CorruptManifest)?;
    if edges.len() != edge_count {
        return Err(MissingEdge);
    }
    let mut q=conn.prepare("SELECT run_id,incarnation,intent_revision,snapshot_json,resume,record_json FROM recovery_members WHERE manifest_id=?1 ORDER BY run_id").map_err(|_|CorruptManifest)?;
    let members = q
        .query_map([id], |r| {
            let snapshot: Option<String> = r.get(3)?;
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, u64>(2)?,
                snapshot,
                r.get::<_, bool>(4)?,
                r.get::<_, String>(5)?,
            ))
        })
        .map_err(|_| CorruptManifest)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| CorruptMember)?
        .into_iter()
        .map(
            |(run_id, incarnation, intent_revision, snapshot, resume, record_json)| {
                let snapshot = snapshot
                    .map(|s| serde_json::from_str(&s))
                    .transpose()
                    .map_err(|_| CorruptMember)?;
                Ok(RecoveryMemberRecord {
                    run_id,
                    incarnation,
                    intent_revision,
                    snapshot,
                    resume,
                    record_json,
                })
            },
        )
        .collect::<Result<Vec<_>, RecoveryProblem>>()?;
    if members.len() != count || count == 0 {
        return Err(MissingMember);
    }
    let ids: BTreeSet<_> = members.iter().map(|m| m.run_id.as_str()).collect();
    if edges
        .iter()
        .any(|e| !ids.contains(e.from.as_str()) || !ids.contains(e.to.as_str()))
    {
        return Err(MissingEdge);
    }
    Ok(RecoveryManifestRecord {
        manifest_id: id.into(),
        schema_version,
        committed,
        superseded,
        edges,
        members,
    })
}

/// Every graph write is FULL-durable and atomic. A failed write cannot leave a
/// successful-looking commit/claim, or consume a checkpoint as a side effect.
pub(crate) fn write(conn: &Connection, command: Write) -> Result<Written, StorageError> {
    let previous: i64 = conn.pragma_query_value(None, "synchronous", |r| r.get(0))?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    let result = (|| {
        let tx = conn.unchecked_transaction()?;
        let result = match command {
            Write::Prepare(record) => {
                prepare(&tx, &record)?;
                Written::Saved
            }
            Write::Commit(id) => {
                commit(&tx, &id)?;
                Written::Saved
            }
            Write::Claim(request) => Written::Claimed(prepare_claim(&tx, &request)?),
            Write::Validate(id) => {
                validate_claim(&tx, &id)?;
                Written::Saved
            }
        };
        tx.commit()?;
        Ok(result)
    })();
    // Never turn a committed transaction into a retryable error.
    if let Err(error) = conn.pragma_update(None, "synchronous", previous) {
        tracing::warn!(%error,"recovery retained FULL synchronous mode");
    }
    result
}
fn prepare(conn: &Connection, record: &RecoveryManifestRecord) -> Result<(), StorageError> {
    if record.schema_version != 1
        || record.committed
        || record.superseded
        || record.members.is_empty()
    {
        return Err(invalid("invalid preparing manifest"));
    }
    let ids: BTreeSet<_> = record.members.iter().map(|m| m.run_id.as_str()).collect();
    if ids.len() != record.members.len()
        || record
            .edges
            .iter()
            .any(|e| !ids.contains(e.from.as_str()) || !ids.contains(e.to.as_str()))
    {
        return Err(invalid("invalid recovery membership"));
    }
    for m in &record.members {
        exact_json(&m.record_json)?;
    }
    // Only unfinished captures overlapping this component are obsolete.
    for m in &record.members {
        conn.execute("DELETE FROM recovery_manifests WHERE committed=0 AND manifest_id IN (SELECT manifest_id FROM recovery_members WHERE run_id=?1)",[&m.run_id])?;
    }
    conn.execute("INSERT INTO recovery_manifests(manifest_id,schema_version,member_count,edge_count,edges_json) VALUES(?1,?2,?3,?4,?5)",params![record.manifest_id,record.schema_version,record.members.len(),record.edges.len(),serde_json::to_string(&record.edges).map_err(|_|invalid("invalid edges"))?])?;
    for m in &record.members {
        conn.execute("INSERT INTO recovery_members(manifest_id,run_id,incarnation,intent_revision,snapshot_json,resume,record_json) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![record.manifest_id,m.run_id,m.incarnation,m.intent_revision,m.snapshot.as_ref().map(serde_json::to_string).transpose().map_err(|_|invalid("invalid snapshot receipt"))?,m.resume,m.record_json])?;
    }
    Ok(())
}
fn evidence(
    conn: &Connection,
    m: &RecoveryMemberRecord,
    revision: u64,
) -> Result<(), StorageError> {
    let row:Option<(String,u64,String)>=conn.query_row("SELECT incarnation,revision,payload FROM resume_intents WHERE run_id=?1 AND NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=?1)",[&m.run_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((inc, rev, payload)) = row else {
        return Err(invalid("missing or tombstoned recovery member"));
    };
    if inc != m.incarnation || rev != revision {
        return Err(invalid("recovery intent revision conflict"));
    }
    let value = exact_json(&payload)?;
    if value.get("run_id").and_then(|v| v.as_str()) != Some(m.run_id.as_str())
        || value.get("incarnation").and_then(|v| v.as_str()) != Some(inc.as_str())
    {
        return Err(invalid("recovery intent identity mismatch"));
    }
    if m.resume
        && (value.get("active").and_then(|v| v.as_bool()) != Some(true)
            || !value
                .get("stop_reason")
                .is_some_and(serde_json::Value::is_null))
    {
        return Err(invalid("recovery member stopped or terminal"));
    }
    if value
        .get("tool_intent")
        .and_then(|v| v.as_array())
        .is_none_or(|calls| !calls.is_empty())
    {
        return Err(invalid("unconfirmed recovery effects"));
    }
    match &m.snapshot {
        Some(receipt) => {
            let referenced = value
                .pointer("/checkpoint/snapshot")
                .or_else(|| value.get("snapshot"))
                .cloned()
                .and_then(|value| serde_json::from_value::<RunContextReceipt>(value).ok());
            if referenced.as_ref() != Some(receipt) {
                return Err(invalid("intent snapshot receipt mismatch"));
            }
            let snapshot = crate::repo::run_context::get(conn, &m.run_id)?
                .ok_or_else(|| invalid("missing recovery snapshot"))?;
            if !receipt.matches(&snapshot) {
                return Err(invalid("recovery snapshot changed"));
            }
            let descriptor: serde_json::Value = serde_json::from_str(&snapshot.config_json)
                .map_err(|_| invalid("corrupt recovery descriptor"))?;
            if descriptor
                .get("non_restorable_reason")
                .and_then(|v| v.as_str())
                == Some("snapshot_consumed")
            {
                return Err(invalid("recovery snapshot consumed"));
            }
        }
        None => {
            if m.resume
                && (value
                    .get("initial_input")
                    .is_none_or(serde_json::Value::is_null)
                    || !matches!(
                        value.pointer("/wait/kind").and_then(|v| v.as_str()),
                        Some("admission" | "admission_queued")
                    ))
            {
                return Err(invalid("recovery member has no safe snapshot"));
            }
        }
    }
    Ok(())
}
fn commit(conn: &Connection, id: &str) -> Result<(), StorageError> {
    let record = load(conn, id).map_err(|_| invalid("corrupt recovery manifest"))?;
    if record.committed || record.superseded {
        return Err(invalid("manifest is not preparing"));
    }
    for m in &record.members {
        evidence(conn, m, m.intent_revision)?;
    }
    for m in &record.members {
        conn.execute("UPDATE recovery_manifests SET superseded=1 WHERE committed=1 AND manifest_id!=?1 AND manifest_id IN (SELECT manifest_id FROM recovery_members WHERE run_id=?2)",params![id,m.run_id])?;
    }
    conn.execute(
        "UPDATE recovery_manifests SET committed=1 WHERE manifest_id=?1",
        [id],
    )?;
    Ok(())
}
fn prepare_claim(
    conn: &Connection,
    request: &RecoveryClaimRequest,
) -> Result<RecoveryClaim, StorageError> {
    let record =
        load(conn, &request.manifest_id).map_err(|_| invalid("corrupt recovery manifest"))?;
    if !record.committed || record.superseded {
        return Err(invalid("manifest is not committed"));
    }
    let candidates: Vec<_> = record.members.iter().filter(|m| m.resume).collect();
    if candidates.is_empty() || candidates.len() != request.incarnations.len() {
        return Err(invalid("invalid recovery claim membership"));
    }
    let ids: BTreeSet<_> = request
        .incarnations
        .iter()
        .map(|(id, _)| id.as_str())
        .collect();
    if ids.len() != candidates.len() {
        return Err(invalid("duplicate recovery claim member"));
    }
    for m in &record.members {
        evidence(conn, m, m.intent_revision)?;
    }
    conn.execute(
        "INSERT INTO recovery_claims(claim_id,manifest_id,state) VALUES(?1,?2,'prepared')",
        params![request.claim_id, request.manifest_id],
    )?;
    for m in candidates {
        let new = request
            .incarnations
            .iter()
            .find(|(id, _)| id == &m.run_id)
            .map(|(_, new)| new)
            .filter(|new| !new.is_empty() && *new != &m.incarnation)
            .ok_or_else(|| invalid("missing new recovery incarnation"))?;
        let changed=conn.execute("UPDATE resume_intents SET revision=revision+1 WHERE run_id=?1 AND incarnation=?2 AND revision=?3",params![m.run_id,m.incarnation,m.intent_revision])?;
        if changed != 1 {
            return Err(invalid("recovery claim CAS conflict"));
        }
        conn.execute("INSERT INTO recovery_claim_members(claim_id,run_id,old_incarnation,reserved_revision,new_incarnation) VALUES(?1,?2,?3,?4,?5)",params![request.claim_id,m.run_id,m.incarnation,m.intent_revision+1,new])?;
    }
    claim(conn, &request.claim_id)
}
fn claim(conn: &Connection, id: &str) -> Result<RecoveryClaim, StorageError> {
    let (manifest_id, state, reason): (String, String, Option<String>) = conn.query_row(
        "SELECT manifest_id,state,release_reason FROM recovery_claims WHERE claim_id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let state = match state.as_str() {
        "prepared" => RecoveryClaimState::Prepared,
        "activated" => RecoveryClaimState::Activated,
        "released" => RecoveryClaimState::Released {
            reason: reason.unwrap_or_default(),
        },
        _ => return Err(invalid("invalid recovery claim state")),
    };
    let mut q=conn.prepare("SELECT run_id,old_incarnation,reserved_revision,new_incarnation FROM recovery_claim_members WHERE claim_id=?1 ORDER BY run_id")?;
    let members = q
        .query_map([id], |r| {
            Ok(RecoveryClaimMember {
                run_id: r.get(0)?,
                old_incarnation: r.get(1)?,
                reserved_revision: r.get(2)?,
                new_incarnation: r.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(RecoveryClaim {
        claim_id: id.into(),
        manifest_id,
        state,
        members,
    })
}
fn validate_claim(conn: &Connection, id: &str) -> Result<(), StorageError> {
    let claim = claim(conn, id)?;
    if claim.state != RecoveryClaimState::Prepared {
        return Err(invalid("recovery claim is not prepared"));
    }
    let record =
        load(conn, &claim.manifest_id).map_err(|_| invalid("corrupt recovery manifest"))?;
    if !record.committed || record.superseded {
        return Err(invalid("recovery manifest superseded"));
    }
    for m in &record.members {
        let revision = if m.resume {
            claim
                .members
                .iter()
                .find(|c| c.run_id == m.run_id)
                .ok_or_else(|| invalid("missing recovery claim member"))?
                .reserved_revision
        } else {
            m.intent_revision
        };
        evidence(conn, m, revision)?;
    }
    Ok(())
}
