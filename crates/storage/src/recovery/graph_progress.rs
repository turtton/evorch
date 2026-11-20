//! Durable step observations keyed independently of an administrative operation.
use super::*;
use graph_control::{GraphControlRecord, GraphEffectKey};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphDisposalProgressKey {
    pub manifest_id: String,
    pub restart_id: String,
    pub capture_id: String,
    pub thread_id: String,
    pub owner_generation: u64,
}
impl GraphDisposalProgressKey {
    fn encoded(&self) -> Result<String, StorageError> {
        if [
            &self.manifest_id,
            &self.restart_id,
            &self.capture_id,
            &self.thread_id,
        ]
        .iter()
        .any(|s| s.is_empty())
        {
            return Err(invalid("invalid graph disposal progress key"));
        }
        Ok(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(self).map_err(|_| invalid("invalid progress key"))?)
        ))
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphDisposalStep {
    NotStarted,
    Applied,
    NotApplicable,
    Unknown,
}
impl GraphDisposalStep {
    fn name(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Applied => "applied",
            Self::NotApplicable => "not_applicable",
            Self::Unknown => "unknown",
        }
    }
    fn parse(s: &str) -> Result<Self, StorageError> {
        match s {
            "not_started" => Ok(Self::NotStarted),
            "applied" => Ok(Self::Applied),
            "not_applicable" => Ok(Self::NotApplicable),
            "unknown" => Ok(Self::Unknown),
            _ => Err(invalid("invalid disposal step")),
        }
    }
    fn may_advance(self, next: Self) -> bool {
        self == next
            || match self {
                Self::NotStarted => true,
                Self::Unknown => next != Self::NotStarted,
                Self::Applied | Self::NotApplicable => false,
            }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphThreadProgress {
    pub key: GraphDisposalProgressKey,
    pub revision: u64,
    pub effect: GraphEffectKey,
    pub owners: GraphDisposalStep,
    pub scratch: GraphDisposalStep,
}
#[derive(Debug, Clone)]
pub struct GraphThreadProgressWrite {
    pub key: GraphDisposalProgressKey,
    pub expected_revision: u64,
    pub control: GraphControlRecord,
    pub owners: GraphDisposalStep,
    pub scratch: GraphDisposalStep,
}
fn read(
    conn: &Connection,
    key: &GraphDisposalProgressKey,
) -> Result<Option<GraphThreadProgress>, StorageError> {
    let row: Option<(u64,String,String,String,String)> = conn.query_row(
        "SELECT revision,operation_id,attempt_nonce,owners,scratch FROM recovery_graph_thread_progress WHERE disposal_key=?1",
        [key.encoded()?],|r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)),
    ).optional()?;
    row.map(|(revision, operation_id, attempt_nonce, owners, scratch)| {
        Ok(GraphThreadProgress {
            key: key.clone(),
            revision,
            effect: GraphEffectKey {
                operation_id,
                attempt_nonce,
            },
            owners: GraphDisposalStep::parse(&owners)?,
            scratch: GraphDisposalStep::parse(&scratch)?,
        })
    })
    .transpose()
}
impl Database {
    /// Diagnostic readback survives a new operation ID; never yields authority.
    pub fn recovery_graph_thread_progress(
        &self,
        key: &GraphDisposalProgressKey,
    ) -> Result<Option<GraphThreadProgress>, StorageError> {
        read(&self.conn, key)
    }
}
pub(super) fn write(
    conn: &Connection,
    request: &GraphThreadProgressWrite,
) -> Result<GraphThreadProgress, StorageError> {
    let key = &request.key;
    let encoded = key.encoded()?;
    let current = graph_control::control(conn, &key.manifest_id)?
        .ok_or_else(|| invalid("missing graph revocation"))?;
    if current != request.control || !current.revoked {
        return Err(invalid("graph progress control CAS conflict"));
    }
    let tx = restart::load(conn, &key.restart_id)?
        .ok_or_else(|| invalid("missing progress transaction"))?;
    let manifest =
        load(conn, &key.manifest_id).map_err(|_| invalid("unreadable progress graph"))?;
    if tx.metadata.capture_id != key.capture_id
        || !tx
            .components
            .iter()
            .any(|c| c.manifest_id == key.manifest_id)
        || !tx.expected.iter().any(|m| {
            m.thread_id.as_ref() == Some(&key.thread_id)
                && manifest.members.iter().any(|r| r.run_id == m.run_id)
        })
    {
        return Err(invalid("foreign graph thread progress"));
    }
    let prior = read(conn, key)?;
    if prior.as_ref().map_or(0, |p| p.revision) != request.expected_revision
        || prior.as_ref().is_some_and(|p| {
            !p.owners.may_advance(request.owners) || !p.scratch.may_advance(request.scratch)
        })
    {
        return Err(invalid("graph progress CAS conflict"));
    }
    let revision = request
        .expected_revision
        .checked_add(1)
        .ok_or_else(|| invalid("progress revision exhausted"))?;
    conn.execute("INSERT INTO recovery_graph_thread_progress(disposal_key,manifest_id,restart_id,capture_id,thread_id,owner_generation,revision,operation_id,attempt_nonce,owners,scratch) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) ON CONFLICT(disposal_key) DO UPDATE SET revision=excluded.revision,operation_id=excluded.operation_id,attempt_nonce=excluded.attempt_nonce,owners=excluded.owners,scratch=excluded.scratch",
        params![encoded,key.manifest_id,key.restart_id,key.capture_id,key.thread_id,key.owner_generation,revision,current.effect.operation_id,current.effect.attempt_nonce,request.owners.name(),request.scratch.name()])?;
    Ok(GraphThreadProgress {
        key: key.clone(),
        revision,
        effect: current.effect,
        owners: request.owners,
        scratch: request.scratch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn internal_raw_ids_are_not_wire_admin_ids() {
        let key = GraphDisposalProgressKey {
            manifest_id: format!("{}/run", "capture".repeat(30)),
            restart_id: "tx".into(),
            capture_id: "capture".repeat(30),
            thread_id: "thread".repeat(30),
            owner_generation: 1,
        };
        assert!(key.manifest_id.len() > 128);
        assert_eq!(key.encoded().unwrap().len(), 64);
    }
}
