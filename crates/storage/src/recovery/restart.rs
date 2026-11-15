//! Durable restart transaction. Recipes are evidence, never execution authority.
use super::*;
use sha2::{Digest, Sha256};

pub const MAX_BUILTIN_TOOL_NAMES: usize = 4096;
pub const MAX_BUILTIN_TOOL_NAME_BYTES: usize = 256;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartMetadata {
    pub restart_id: String,
    pub capture_id: String,
    pub config_identity: String,
    pub old_epoch: u64,
    pub successor_epoch: u64,
    pub old_build: String,
    pub successor_build: String,
    pub target_identity: String,
    pub handoff_version: u32,
    pub snapshot_version: u32,
    pub storage_version: u32,
    pub old_builtin_tool_names: BTreeSet<String>,
    pub old_compiled_catalog_fingerprint: Option<String>,
    pub new_compiled_catalog_fingerprint: Option<String>,
    pub old_effective_tools_fingerprint: Option<String>,
    pub new_effective_tools_fingerprint: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartPhase {
    Preparing,
    Capturing,
    Committed,
    Armed,
    Adopted,
    Completed,
    Aborted,
    Invalidated,
    Superseded,
}
impl RestartPhase {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Aborted | Self::Invalidated | Self::Superseded
        )
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartExpectedMember {
    pub run_id: String,
    pub incarnation: String,
    pub root_run_id: String,
    pub thread_id: Option<String>,
    pub intent_revision: u64,
    pub snapshot: Option<RunContextReceipt>,
    pub owner_id: Option<String>,
    pub owner_generation: Option<u64>,
    pub lane_id: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartDisposition {
    RetryPending,
    Activated,
    Released,
    Quarantined,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartComponent {
    pub manifest_id: String,
    pub disposition: RestartDisposition,
    pub claim_id: Option<String>,
    pub reason: Option<String>,
    pub revision: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartOwnerProgress {
    pub thread_id: String,
    pub old_owner_id: String,
    pub old_generation: u64,
    pub active_runs: BTreeSet<String>,
    pub hold_saved: bool,
    pub hold_read_back: bool,
    pub worker_joined: bool,
    pub socket_unlinked: bool,
    pub hold_removed: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartTransaction {
    pub metadata: RestartMetadata,
    pub revision: u64,
    pub phase: RestartPhase,
    pub expected: Vec<RestartExpectedMember>,
    pub expected_digest: Option<String>,
    pub manifest_digest: Option<String>,
    pub components: Vec<RestartComponent>,
    pub owner_progress: Vec<RestartOwnerProgress>,
    pub ticket_nonce_hash: Option<String>,
    pub adopted_epoch: Option<u64>,
    pub ready_at_ms: Option<u64>,
    pub failure_reason: Option<String>,
}
impl RestartTransaction {
    pub fn retry_pending(&self) -> usize {
        self.components
            .iter()
            .filter(|c| c.disposition == RestartDisposition::RetryPending)
            .count()
    }
}
#[derive(Debug)]
pub enum RestartMutation {
    Create(Box<RestartMetadata>),
    BeginCapture(Vec<RestartExpectedMember>),
    SaveComponent(RecoveryManifestRecord),
    #[cfg(any(test, feature = "recovery-test-support"))]
    BindComponent {
        manifest_id: String,
    },
    Commit,
    Arm {
        nonce_hash: String,
    },
    Adopt {
        nonce_hash: String,
        successor_epoch: u64,
        config_identity: String,
        successor_build: String,
    },
    OwnerProgress(RestartOwnerProgress),
    Dispose {
        manifest_id: String,
        expected_revision: u64,
        disposition: RestartDisposition,
        claim_id: Option<String>,
        reason: Option<String>,
    },
    Ready {
        at_ms: u64,
    },
    Abort {
        reason: String,
    },
    Invalidate {
        reason: String,
    },
    Supersede {
        reason: String,
    },
    QuarantineCold,
}
fn encode<T: Serialize>(v: &T) -> Result<String, StorageError> {
    serde_json::to_string(v).map_err(|_| invalid("invalid restart evidence"))
}
fn decode<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, StorageError> {
    serde_json::from_str(s).map_err(|_| invalid("corrupt restart evidence"))
}
fn tag<T: Serialize>(v: &T) -> Result<String, StorageError> {
    Ok(encode(v)?.trim_matches('"').to_owned())
}
pub fn restart_digest<T: Serialize>(v: &T) -> Result<String, StorageError> {
    Ok(format!("{:x}", Sha256::digest(encode(v)?.as_bytes())))
}
fn phase(p: RestartPhase, expected: RestartPhase) -> Result<(), StorageError> {
    if p == expected {
        Ok(())
    } else {
        Err(invalid("restart phase conflict"))
    }
}

impl Database {
    /// Minimal CAS identity remains readable even if a graph payload is corrupt.
    pub fn restart_revision_phase(
        &self,
        id: &str,
    ) -> Result<Option<(u64, RestartPhase)>, StorageError> {
        let row: Option<(u64, String)> = self
            .conn
            .query_row(
                "SELECT revision,phase FROM restart_transactions WHERE restart_id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(r, p)| Ok((r, decode(&format!("\"{p}\""))?)))
            .transpose()
    }
    pub fn validate_restart_capture_digest(&self, id: &str) -> Result<(), StorageError> {
        let t = load(&self.conn, id)?.ok_or_else(|| invalid("missing restart"))?;
        if t.manifest_digest.as_deref() != Some(&manifest_digest(&self.conn, &t)?) {
            return Err(invalid("restart manifest digest changed"));
        }
        Ok(())
    }

    pub fn restart_transaction(
        &self,
        id: &str,
    ) -> Result<Option<RestartTransaction>, StorageError> {
        load(&self.conn, id)
    }
    /// Only relational IDs/revisions: corrupt payloads must not block unrelated work.
    pub fn cold_restart_candidates(&self) -> Result<Vec<(String, u64)>, StorageError> {
        let mut q=self.conn.prepare("SELECT restart_id,revision FROM restart_transactions t WHERE phase != 'completed' AND (phase NOT IN ('aborted','invalidated','superseded') OR EXISTS (SELECT 1 FROM restart_components c WHERE c.restart_id=t.restart_id AND c.disposition='retry_pending')) ORDER BY restart_id")?;
        Ok(q.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?)
    }
    pub fn restart_transactions(&self) -> Result<Vec<RestartTransaction>, StorageError> {
        let mut q = self
            .conn
            .prepare("SELECT restart_id FROM restart_transactions ORDER BY restart_id")?;
        let ids = q
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| load(&self.conn, &id)?.ok_or_else(|| invalid("restart disappeared")))
            .collect()
    }
    /// Relational identities remain available after a partial/corrupt capture or manifest deletion.
    pub fn restart_excluded_run_ids(&self) -> Result<BTreeSet<String>, StorageError> {
        let mut q=self.conn.prepare("SELECT run_id FROM restart_expected_members UNION SELECT root_run_id FROM restart_expected_members")?;
        Ok(q.query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?)
    }
    pub fn restart_for_manifest(
        &self,
        manifest: &str,
    ) -> Result<Option<RestartTransaction>, StorageError> {
        let id: Option<String> = self
            .conn
            .query_row(
                "SELECT restart_id FROM restart_components WHERE manifest_id=?1",
                [manifest],
                |r| r.get(0),
            )
            .optional()?;
        id.map(|id| load(&self.conn, &id)?.ok_or_else(|| invalid("missing restart membership")))
            .transpose()
    }
}
pub(super) fn load(
    conn: &Connection,
    id: &str,
) -> Result<Option<RestartTransaction>, StorageError> {
    let row=conn.query_row("SELECT metadata_json,revision,phase,expected_digest,manifest_digest,ticket_nonce_hash,adopted_epoch,ready_at_ms,failure_reason FROM restart_transactions WHERE restart_id=?1",[id],|r|Ok((r.get::<_,String>(0)?,r.get(1)?,r.get::<_,String>(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?))).optional()?;
    let Some((
        metadata,
        revision,
        p,
        expected_digest,
        manifest_digest,
        ticket_nonce_hash,
        adopted_epoch,
        ready_at_ms,
        failure_reason,
    )) = row
    else {
        return Ok(None);
    };
    let mut q = conn.prepare(
        "SELECT run_id,root_run_id,member_json FROM restart_expected_members WHERE restart_id=?1 ORDER BY run_id",
    )?;
    let expected = q
        .query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|(run, root, v)| {
            let member: RestartExpectedMember = decode(&v)?;
            if member.run_id != run || member.root_run_id != root {
                return Err(invalid("restart relational identity changed"));
            }
            Ok(member)
        })
        .collect::<Result<Vec<RestartExpectedMember>, _>>()?;
    if let Some(digest) = &expected_digest
        && restart_digest(&expected)? != *digest
    {
        return Err(invalid("restart expected set changed"));
    }
    let mut q=conn.prepare("SELECT manifest_id,disposition,claim_id,reason,revision FROM restart_components WHERE restart_id=?1 ORDER BY manifest_id")?;
    let raw = q
        .query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let components = raw
        .into_iter()
        .map(|(manifest_id, d, claim_id, reason, revision)| {
            Ok(RestartComponent {
                manifest_id,
                disposition: decode(&format!("\"{d}\""))?,
                claim_id,
                reason,
                revision,
            })
        })
        .collect::<Result<Vec<_>, StorageError>>()?;
    let mut q = conn.prepare(
        "SELECT progress_json FROM restart_owner_progress WHERE restart_id=?1 ORDER BY thread_id",
    )?;
    let owner_progress = q
        .query_map([id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|s| decode(&s))
        .collect::<Result<_, _>>()?;
    Ok(Some(RestartTransaction {
        metadata: decode(&metadata)?,
        revision,
        phase: decode(&format!("\"{p}\""))?,
        expected,
        expected_digest,
        manifest_digest,
        components,
        owner_progress,
        ticket_nonce_hash,
        adopted_epoch,
        ready_at_ms,
        failure_reason,
    }))
}

pub(super) fn mutate(
    conn: &Connection,
    id: &str,
    expected_revision: Option<u64>,
    mutation: RestartMutation,
) -> Result<RestartTransaction, StorageError> {
    if let RestartMutation::Create(m) = mutation {
        if expected_revision.is_some()
            || m.restart_id != id
            || id.is_empty()
            || m.capture_id.is_empty()
            || m.config_identity.is_empty()
            || m.old_build.is_empty()
            || m.successor_build.is_empty()
            || m.target_identity.is_empty()
            || m.handoff_version == 0
            || m.snapshot_version == 0
            || m.storage_version == 0
            || m.old_epoch == 0
            || m.successor_epoch == 0
            || m.old_epoch == m.successor_epoch
            || m.old_builtin_tool_names.len() > MAX_BUILTIN_TOOL_NAMES
            || m.old_builtin_tool_names
                .iter()
                .any(|n| n.is_empty() || n.len() > MAX_BUILTIN_TOOL_NAME_BYTES)
        {
            return Err(invalid("invalid restart metadata"));
        }
        let pending: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM restart_transactions t JOIN restart_components c USING(restart_id) WHERE t.phase='adopted' AND c.disposition='retry_pending' AND json_extract(t.metadata_json,'$.config_identity')=?1)",
            [&m.config_identity],
            |r| r.get(0),
        )?;
        if pending {
            return Err(invalid("previous adopted restart has pending components"));
        }
        exact_json(&encode(&m)?)?;
        conn.execute("INSERT INTO restart_transactions(restart_id,capture_id,revision,phase,metadata_json,old_builtin_tool_names_json,old_compiled_catalog_fingerprint,new_compiled_catalog_fingerprint,old_effective_tools_fingerprint,new_effective_tools_fingerprint) VALUES(?1,?2,0,'preparing',?3,?4,?5,?6,?7,?8)",params![id,m.capture_id,encode(&m)?,encode(&m.old_builtin_tool_names)?,m.old_compiled_catalog_fingerprint,m.new_compiled_catalog_fingerprint,m.old_effective_tools_fingerprint,m.new_effective_tools_fingerprint])?;
        return load(conn, id)?.ok_or_else(|| invalid("restart create missing"));
    }
    let mut tx = load(conn, id)?.ok_or_else(|| invalid("missing restart transaction"))?;
    if expected_revision != Some(tx.revision) {
        return Err(invalid("restart revision conflict; read back same ID"));
    }
    match mutation {
        RestartMutation::Create(_) => unreachable!(),
        RestartMutation::BeginCapture(mut members) => {
            phase(tx.phase, RestartPhase::Preparing)?;
            members.sort_by(|a, b| a.run_id.cmp(&b.run_id));
            exact_json(&encode(&members)?)?;
            let ids: BTreeSet<_> = members.iter().map(|m| m.run_id.as_str()).collect();
            if ids.len() != members.len() {
                return Err(invalid("duplicate expected member"));
            }
            for m in &members {
                if !ids.contains(m.root_run_id.as_str()) || m.incarnation.is_empty() {
                    return Err(invalid("invalid expected member"));
                }
                conn.execute("INSERT INTO restart_expected_members(restart_id,run_id,root_run_id,member_json) VALUES(?1,?2,?3,?4)",params![id,m.run_id,m.root_run_id,encode(m)?])?;
            }
            conn.execute(
                "UPDATE restart_transactions SET expected_digest=?2 WHERE restart_id=?1",
                params![id, restart_digest(&members)?],
            )?;
            tx.phase = RestartPhase::Capturing;
        }
        RestartMutation::SaveComponent(record) => {
            phase(tx.phase, RestartPhase::Capturing)?;
            for m in &record.members {
                let expected = tx
                    .expected
                    .iter()
                    .find(|e| e.run_id == m.run_id)
                    .ok_or_else(|| invalid("unexpected restart member"))?;
                if expected.incarnation != m.incarnation
                    || expected.intent_revision != m.intent_revision
                    || expected.snapshot != m.snapshot
                {
                    return Err(invalid("restart member evidence mismatch"));
                }
                let assigned:Option<String>=conn.query_row("SELECT manifest_id FROM restart_expected_members WHERE restart_id=?1 AND run_id=?2",params![id,m.run_id],|r|r.get(0))?;
                if assigned.is_some() {
                    return Err(invalid("restart member already captured"));
                }
            }
            super::prepare(conn, &record)?;
            super::commit(conn, &record.manifest_id)?;
            conn.execute("INSERT INTO restart_components(restart_id,manifest_id,disposition) VALUES(?1,?2,'retry_pending')",params![id,record.manifest_id])?;
            for m in &record.members {
                conn.execute("UPDATE restart_expected_members SET manifest_id=?3 WHERE restart_id=?1 AND run_id=?2",params![id,m.run_id,record.manifest_id])?;
            }
        }
        #[cfg(any(test, feature = "recovery-test-support"))]
        RestartMutation::BindComponent { manifest_id } => {
            phase(tx.phase, RestartPhase::Capturing)?;
            let record =
                super::load(conn, &manifest_id).map_err(|_| invalid("corrupt bound manifest"))?;
            if !record.committed || record.superseded {
                return Err(invalid("bound manifest is not committed"));
            }
            for m in &record.members {
                let e = tx
                    .expected
                    .iter()
                    .find(|e| e.run_id == m.run_id)
                    .ok_or_else(|| invalid("unexpected bound member"))?;
                if e.incarnation != m.incarnation
                    || e.intent_revision != m.intent_revision
                    || e.snapshot != m.snapshot
                {
                    return Err(invalid("bound member mismatch"));
                }
                super::evidence(conn, m, m.intent_revision)?;
                let changed=conn.execute("UPDATE restart_expected_members SET manifest_id=?3 WHERE restart_id=?1 AND run_id=?2 AND manifest_id IS NULL",params![id,m.run_id,manifest_id])?;
                if changed != 1 {
                    return Err(invalid("bound member already assigned"));
                }
            }
            conn.execute("INSERT INTO restart_components(restart_id,manifest_id,disposition) VALUES(?1,?2,'retry_pending')",params![id,manifest_id])?;
        }
        RestartMutation::Commit => {
            phase(tx.phase, RestartPhase::Capturing)?;
            validate_capture(conn, &tx)?;
            let digest = manifest_digest(conn, &tx)?;
            conn.execute(
                "UPDATE restart_transactions SET manifest_digest=?2 WHERE restart_id=?1",
                params![id, digest],
            )?;
            tx.phase = RestartPhase::Committed;
        }
        RestartMutation::Arm { nonce_hash } => {
            phase(tx.phase, RestartPhase::Committed)?;
            validate_capture(conn, &tx)?;
            if nonce_hash.is_empty() {
                return Err(invalid("empty handoff nonce"));
            }
            conn.execute(
                "UPDATE restart_transactions SET ticket_nonce_hash=?2 WHERE restart_id=?1",
                params![id, nonce_hash],
            )?;
            tx.phase = RestartPhase::Armed;
        }
        RestartMutation::Adopt {
            nonce_hash,
            successor_epoch,
            config_identity,
            successor_build,
        } => {
            phase(tx.phase, RestartPhase::Armed)?;
            if tx.ticket_nonce_hash.as_deref() != Some(&nonce_hash)
                || tx.metadata.successor_epoch != successor_epoch
                || tx.metadata.config_identity != config_identity
                || tx.metadata.successor_build != successor_build
            {
                return Err(invalid("handoff ticket mismatch"));
            }
            validate_capture(conn, &tx)?;
            if tx.manifest_digest.as_ref() != Some(&manifest_digest(conn, &tx)?) {
                return Err(invalid("restart manifest digest changed"));
            }
            conn.execute(
                "UPDATE restart_transactions SET adopted_epoch=?2 WHERE restart_id=?1",
                params![id, successor_epoch],
            )?;
            tx.phase = RestartPhase::Adopted;
        }
        RestartMutation::OwnerProgress(p) => {
            if !matches!(
                tx.phase,
                RestartPhase::Committed
                    | RestartPhase::Armed
                    | RestartPhase::Adopted
                    | RestartPhase::Invalidated
            ) {
                return Err(invalid("owner progress outside handoff"));
            }
            if p.thread_id.is_empty()
                || p.old_owner_id.is_empty()
                || p.old_generation == 0
                || (p.hold_read_back && !p.hold_saved)
                || (p.worker_joined && !p.hold_read_back)
                || (p.socket_unlinked && !p.worker_joined)
                || (p.hold_removed && !p.hold_saved)
            {
                return Err(invalid("invalid owner progress ordering"));
            }
            if p.active_runs.iter().any(|id| {
                !tx.expected.iter().any(|e| {
                    e.run_id == *id
                        && e.thread_id.as_deref() == Some(&p.thread_id)
                        && e.owner_id
                            .as_deref()
                            .is_none_or(|owner| owner == p.old_owner_id)
                        && e.owner_generation.is_none_or(|g| g == p.old_generation)
                })
            }) {
                return Err(invalid("owner progress membership mismatch"));
            }
            if let Some(old) = tx
                .owner_progress
                .iter()
                .find(|o| o.thread_id == p.thread_id)
                && (old.old_owner_id != p.old_owner_id
                    || old.old_generation != p.old_generation
                    || old.active_runs != p.active_runs
                    || (old.hold_saved && !p.hold_saved)
                    || (old.hold_read_back && !p.hold_read_back)
                    || (old.worker_joined && !p.worker_joined)
                    || (old.socket_unlinked && !p.socket_unlinked)
                    || (old.hold_removed && !p.hold_removed))
            {
                return Err(invalid("owner progress regressed"));
            }
            exact_json(&encode(&p)?)?;
            conn.execute("INSERT INTO restart_owner_progress VALUES(?1,?2,?3) ON CONFLICT(restart_id,thread_id) DO UPDATE SET progress_json=excluded.progress_json",params![id,p.thread_id,encode(&p)?])?;
        }
        RestartMutation::Dispose {
            manifest_id,
            expected_revision,
            disposition,
            claim_id,
            reason,
        } => {
            phase(tx.phase, RestartPhase::Adopted)?;
            let c = tx
                .components
                .iter()
                .find(|c| c.manifest_id == manifest_id)
                .ok_or_else(|| invalid("unknown restart component"))?;
            if c.revision != expected_revision
                || c.disposition != RestartDisposition::RetryPending
                || disposition == RestartDisposition::RetryPending
            {
                return Err(invalid("component disposition conflict"));
            }
            if let Some(reason) = &reason {
                if reason.len() > 2048 {
                    return Err(invalid("component reason too long"));
                }
                exact_json(&encode(reason)?)?;
            }
            if disposition == RestartDisposition::Released {
                let claim = super::claim(
                    conn,
                    claim_id
                        .as_deref()
                        .ok_or_else(|| invalid("missing release claim"))?,
                )?;
                if claim.manifest_id != manifest_id
                    || !matches!(claim.state, RecoveryClaimState::Released { .. })
                {
                    return Err(invalid("component not released"));
                }
            }
            if disposition == RestartDisposition::Activated {
                let claim = super::claim(
                    conn,
                    claim_id
                        .as_deref()
                        .ok_or_else(|| invalid("missing activation claim"))?,
                )?;
                if claim.manifest_id != manifest_id || claim.state != RecoveryClaimState::Activated
                {
                    return Err(invalid("component not activated"));
                }
            }
            conn.execute("UPDATE restart_components SET disposition=?3,claim_id=?4,reason=?5,revision=revision+1 WHERE restart_id=?1 AND manifest_id=?2",params![id,manifest_id,tag(&disposition)?,claim_id,reason])?;
        }
        RestartMutation::Ready { at_ms } => {
            phase(tx.phase, RestartPhase::Adopted)?;
            if tx.ready_at_ms.is_some() {
                return Err(invalid("Ready already recorded"));
            }
            conn.execute(
                "UPDATE restart_transactions SET ready_at_ms=?2 WHERE restart_id=?1",
                params![id, at_ms],
            )?;
        }
        RestartMutation::Abort { reason } => {
            phase(tx.phase, RestartPhase::Preparing)?;
            tx.phase = RestartPhase::Aborted;
            fail(conn, id, &reason)?;
        }
        RestartMutation::Invalidate { reason } => {
            if tx.phase.terminal() {
                return Err(invalid("restart already terminal"));
            }
            tx.phase = RestartPhase::Invalidated;
            fail(conn, id, &reason)?;
        }
        RestartMutation::Supersede { reason } => {
            if tx.phase.terminal() {
                return Err(invalid("restart already terminal"));
            }
            tx.phase = RestartPhase::Superseded;
            fail(conn, id, &reason)?;
        }
        RestartMutation::QuarantineCold => {
            if tx.phase == RestartPhase::Completed {
                return Ok(tx);
            }
            if tx.phase == RestartPhase::Preparing {
                tx.phase = RestartPhase::Aborted;
            } else {
                tx.phase = RestartPhase::Invalidated;
            }
            conn.execute("UPDATE restart_components SET disposition='quarantined',reason='cold boot has no inherited proof',revision=revision+1 WHERE restart_id=?1 AND disposition='retry_pending'",[id])?;
            fail(conn, id, "cold boot has no inherited proof")?;
        }
    }
    let current = load(conn, id)?.ok_or_else(|| invalid("restart disappeared"))?;
    if tx.phase == RestartPhase::Adopted
        && current.ready_at_ms.is_some()
        && current.retry_pending() == 0
    {
        tx.phase = RestartPhase::Completed;
    }
    if conn.execute("UPDATE restart_transactions SET phase=?2,revision=revision+1 WHERE restart_id=?1 AND revision=?3",params![id,tag(&tx.phase)?,expected_revision])?!=1 {return Err(invalid("restart revision changed during mutation"))}
    load(conn, id)?.ok_or_else(|| invalid("restart update missing"))
}
fn fail(conn: &Connection, id: &str, reason: &str) -> Result<(), StorageError> {
    if reason.len() > 2048 {
        return Err(invalid("restart reason too long"));
    }
    exact_json(&encode(&reason)?)?;
    conn.execute(
        "UPDATE restart_transactions SET failure_reason=?2 WHERE restart_id=?1",
        params![id, reason],
    )?;
    Ok(())
}
fn manifest_digest(conn: &Connection, tx: &RestartTransaction) -> Result<String, StorageError> {
    let records = tx
        .components
        .iter()
        .map(|c| super::load(conn, &c.manifest_id).map_err(|_| invalid("corrupt restart manifest")))
        .collect::<Result<Vec<_>, _>>()?;
    restart_digest(
        &records
            .iter()
            .map(|m| {
                (
                    &m.manifest_id,
                    m.schema_version,
                    &m.edges,
                    m.members
                        .iter()
                        .map(|m| {
                            (
                                &m.run_id,
                                &m.incarnation,
                                m.resume,
                                m.intent_revision,
                                &m.snapshot,
                                &m.record_json,
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>(),
    )
}
fn validate_capture(conn: &Connection, tx: &RestartTransaction) -> Result<(), StorageError> {
    let mut actual = BTreeSet::new();
    for c in &tx.components {
        let m =
            super::load(conn, &c.manifest_id).map_err(|_| invalid("corrupt restart manifest"))?;
        if !m.committed || m.superseded {
            return Err(invalid("restart manifest not committed"));
        }
        for member in &m.members {
            let expected = tx
                .expected
                .iter()
                .find(|e| e.run_id == member.run_id)
                .ok_or_else(|| invalid("unexpected captured member"))?;
            if !actual.insert(member.run_id.clone())
                || expected.incarnation != member.incarnation
                || expected.intent_revision != member.intent_revision
                || expected.snapshot != member.snapshot
            {
                return Err(invalid("captured member differs from expected"));
            }
            super::evidence(conn, member, member.intent_revision)?;
        }
    }
    if actual != tx.expected.iter().map(|m| m.run_id.clone()).collect() {
        return Err(invalid("incomplete restart capture"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;

/// Participate in the same FULL activation/release transaction. Normal
/// activation consumes membership instead of superseding its transaction.
pub(super) fn claim_disposition(
    conn: &Connection,
    claim: &RecoveryClaim,
    disposition: RestartDisposition,
    reason: Option<&str>,
) -> Result<(), StorageError> {
    let id:Option<String>=conn.query_row("SELECT restart_id FROM restart_components WHERE manifest_id=?1 AND disposition='retry_pending'",[&claim.manifest_id],|r|r.get(0)).optional()?;
    let Some(id) = id else { return Ok(()) };
    let tx = load(conn, &id)?.ok_or_else(|| invalid("restart membership missing"))?;
    if tx.phase != RestartPhase::Adopted {
        return Ok(());
    }
    let c = tx
        .components
        .iter()
        .find(|c| c.manifest_id == claim.manifest_id)
        .ok_or_else(|| invalid("component membership missing"))?;
    mutate(
        conn,
        &id,
        Some(tx.revision),
        RestartMutation::Dispose {
            manifest_id: claim.manifest_id.clone(),
            expected_revision: c.revision,
            disposition,
            claim_id: Some(claim.claim_id.clone()),
            reason: reason.map(str::to_owned),
        },
    )?;
    Ok(())
}

/// This minimal FULL/CAS write must remain available when decoding failed.
pub(super) fn invalidate(
    conn: &Connection,
    id: &str,
    revision: u64,
    reason: &str,
) -> Result<(), StorageError> {
    fail(conn, id, reason)?;
    if conn.execute("UPDATE restart_transactions SET phase='invalidated',revision=revision+1 WHERE restart_id=?1 AND revision=?2 AND phase NOT IN ('completed','aborted','invalidated','superseded')",params![id,revision])?!=1{return Err(invalid("restart invalidation CAS conflict"))}
    Ok(())
}

/// Minimal cold disposition is independent of payload parsing, just like invalidation.
pub(super) fn quarantine_cold(
    conn: &Connection,
    id: &str,
    revision: u64,
) -> Result<(), StorageError> {
    let changed=conn.execute("UPDATE restart_transactions SET phase=CASE WHEN phase='preparing' THEN 'aborted' ELSE 'invalidated' END,revision=revision+1,failure_reason='cold boot has no inherited proof' WHERE restart_id=?1 AND revision=?2 AND phase != 'completed'",params![id,revision])?;
    if changed != 1 {
        return Err(invalid("restart cold quarantine revision conflict"));
    }
    conn.execute("UPDATE restart_components SET disposition='quarantined',reason='cold boot has no inherited proof',revision=revision+1 WHERE restart_id=?1 AND disposition='retry_pending'",[id])?;
    Ok(())
}
