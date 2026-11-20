//! Writer-side Q2 eligibility CAS. Records are observations, not executable tokens.
use super::*;
use restart::{RestartComponent, RestartDisposition, RestartExpectedMember, RestartPhase};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphEffectKey {
    pub operation_id: String,
    pub attempt_nonce: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphControlCas {
    pub restart_id: String,
    pub capture_id: String,
    pub successor_epoch: u64,
    pub phase: RestartPhase,
    pub component: RestartComponent,
    pub membership: Vec<RestartExpectedMember>,
    pub manifest: RecoveryManifestRecord,
    pub claim: Option<RecoveryClaim>,
    /// Zero expects no control row; it is not a wildcard.
    pub control_revision: u64,
    pub control_effect: Option<GraphEffectKey>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphControlAction {
    Resume,
    Release,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphControlWrite {
    pub expected: GraphControlCas,
    pub effect: GraphEffectKey,
    pub action: GraphControlAction,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphControlRecord {
    pub manifest_id: String,
    pub mode: String,
    pub revision: u64,
    pub effect: GraphEffectKey,
    pub revoked: bool,
    pub origin: String,
}
/// This expectation is not authority. Runtime's non-Clone reservation supplies
/// it to the writer, which checks it inside the activation transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationControl {
    Automatic,
    Manual {
        effect: GraphEffectKey,
        control_revision: u64,
    },
}

fn encode<T: Serialize>(value: &T) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|_| invalid("invalid graph control record"))
}
pub(super) fn control(
    conn: &Connection,
    manifest: &str,
) -> Result<Option<GraphControlRecord>, StorageError> {
    Ok(conn.query_row(
        "SELECT mode,revision,operation_id,attempt_nonce,revoked,origin FROM recovery_graph_control WHERE manifest_id=?1",
        [manifest], |r| Ok(GraphControlRecord {
            manifest_id: manifest.into(), mode:r.get(0)?, revision:r.get(1)?,
            effect:GraphEffectKey {operation_id:r.get(2)?,attempt_nonce:r.get(3)?},
            revoked:r.get(4)?,origin:r.get(5)?,
        }),
    ).optional()?)
}
impl Database {
    /// A current-format snapshot only, never a source of runtime authority.
    pub fn recovery_graph_cas(
        &self,
        restart_id: &str,
        manifest_id: &str,
    ) -> Result<GraphControlCas, StorageError> {
        let snapshot = self.conn.unchecked_transaction()?;
        let tx = restart::load(&snapshot, restart_id)?
            .ok_or_else(|| invalid("missing graph transaction"))?;
        if tx.adopted_epoch != Some(tx.metadata.successor_epoch) {
            return Err(invalid("graph was not adopted"));
        }
        let manifest =
            load(&snapshot, manifest_id).map_err(|_| invalid("unreadable graph manifest"))?;
        let component = tx
            .components
            .iter()
            .find(|c| c.manifest_id == manifest_id)
            .cloned()
            .ok_or_else(|| invalid("foreign graph"))?;
        let membership: Vec<_> = tx
            .expected
            .iter()
            .filter(|m| manifest.members.iter().any(|r| r.run_id == m.run_id))
            .cloned()
            .collect();
        if membership.len() != manifest.members.len() {
            return Err(invalid("incomplete graph membership"));
        }
        let ids: Vec<String> = snapshot
            .prepare("SELECT claim_id FROM recovery_claims WHERE manifest_id=?1 ORDER BY claim_id")?
            .query_map([manifest_id], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let claim = match ids.as_slice() {
            [] => None,
            [id] => Some(claim(&snapshot, id)?),
            _ => return Err(invalid("ambiguous graph claim")),
        };
        let control = control(&snapshot, manifest_id)?;
        Ok(GraphControlCas {
            restart_id: restart_id.into(),
            capture_id: tx.metadata.capture_id,
            successor_epoch: tx.metadata.successor_epoch,
            phase: tx.phase,
            component,
            membership,
            manifest,
            claim,
            control_revision: control.as_ref().map_or(0, |c| c.revision),
            control_effect: control.map(|c| c.effect),
        })
    }
    pub fn recovery_graph_control(
        &self,
        manifest: &str,
    ) -> Result<Option<GraphControlRecord>, StorageError> {
        control(&self.conn, manifest)
    }
    /// Independent readback remains possible after the graph CAS has advanced.
    pub fn recovery_graph_effect(
        &self,
        operation: &str,
    ) -> Result<Option<GraphControlRecord>, StorageError> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT result_json FROM recovery_graph_effects WHERE operation_id=?1",
                [operation],
                |r| r.get(0),
            )
            .optional()?;
        json.map(|j| serde_json::from_str(&j).map_err(|_| invalid("unreadable graph effect")))
            .transpose()
    }
}
pub(super) fn check_prepare(conn: &Connection, manifest: &str) -> Result<(), StorageError> {
    if control(conn, manifest)?.is_some() {
        return Err(invalid("graph eligibility reserved or revoked"));
    }
    Ok(())
}
pub(super) fn check_activation(
    conn: &Connection,
    manifest: &str,
    reservation: &ActivationControl,
) -> Result<(), StorageError> {
    match (reservation, control(conn, manifest)?) {
        (ActivationControl::Automatic, None) => Ok(()),
        (
            ActivationControl::Manual {
                effect,
                control_revision,
            },
            Some(c),
        ) if !c.revoked
            && c.mode == "manual_running"
            && c.effect == *effect
            && c.revision == *control_revision =>
        {
            Ok(())
        }
        _ => Err(invalid("activation reservation or eligibility conflict")),
    }
}
pub(super) fn resolve_activation(
    conn: &Connection,
    manifest: &str,
    reservation: &ActivationControl,
) -> Result<(), StorageError> {
    if let ActivationControl::Manual {
        effect,
        control_revision,
    } = reservation
    {
        let n = conn.execute(
            "UPDATE recovery_graph_control SET mode='resolved',revision=revision+1 WHERE manifest_id=?1 AND revision=?2 AND operation_id=?3 AND attempt_nonce=?4 AND revoked=0 AND mode='manual_running'",
            params![manifest,control_revision,effect.operation_id,effect.attempt_nonce],
        )?;
        if n != 1 {
            return Err(invalid("manual activation marker conflict"));
        }
    }
    Ok(())
}
fn check_cas(conn: &Connection, expected: &GraphControlCas) -> Result<(), StorageError> {
    let tx = restart::load(conn, &expected.restart_id)?
        .ok_or_else(|| invalid("missing graph transaction"))?;
    let manifest_id = &expected.manifest.manifest_id;
    let component = tx.components.iter().find(|c| &c.manifest_id == manifest_id);
    let membership: Vec<_> = tx
        .expected
        .iter()
        .filter(|m| {
            expected
                .manifest
                .members
                .iter()
                .any(|r| r.run_id == m.run_id)
        })
        .cloned()
        .collect();
    if tx.metadata.capture_id != expected.capture_id
        || tx.metadata.successor_epoch != expected.successor_epoch
        || tx.adopted_epoch != Some(expected.successor_epoch)
        || tx.phase != expected.phase
        || component != Some(&expected.component)
        || membership != expected.membership
        || membership.len() != expected.manifest.members.len()
        || load(conn, manifest_id).map_err(|_| invalid("unreadable graph manifest"))?
            != expected.manifest
    {
        return Err(invalid("graph CAS conflict"));
    }
    let claims: Vec<String> = conn
        .prepare("SELECT claim_id FROM recovery_claims WHERE manifest_id=?1 ORDER BY claim_id")?
        .query_map([manifest_id], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let actual = match claims.as_slice() {
        [] => None,
        [id] => Some(claim(conn, id)?),
        _ => return Err(invalid("ambiguous graph claim")),
    };
    if actual != expected.claim {
        return Err(invalid("graph claim CAS conflict"));
    }
    match control(conn, manifest_id)? {
        None if expected.control_revision == 0 && expected.control_effect.is_none() => (),
        Some(c)
            if expected.control_revision == c.revision
                && expected.control_effect.as_ref() == Some(&c.effect)
                && matches!(c.mode.as_str(), "manual_idle" | "revoked_idle") => {}
        _ => return Err(invalid("graph control CAS conflict")),
    }
    Ok(())
}
pub(super) fn write(
    conn: &Connection,
    request: &GraphControlWrite,
) -> Result<GraphControlRecord, StorageError> {
    let json = encode(request)?;
    let prior: Option<(String, String)> = conn
        .query_row(
            "SELECT request_json,result_json FROM recovery_graph_effects WHERE operation_id=?1",
            [&request.effect.operation_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((prior, result)) = prior {
        if prior != json {
            return Err(invalid("graph operation identity conflict"));
        }
        return serde_json::from_str(&result).map_err(|_| invalid("unreadable graph effect"));
    }
    for id in [&request.effect.operation_id, &request.effect.attempt_nonce] {
        if id.is_empty() || id.len() > 128 {
            return Err(invalid("invalid graph effect key"));
        }
    }
    check_cas(conn, &request.expected)?;
    let e = &request.expected;
    if e.phase != RestartPhase::Adopted
        || e.component.disposition != RestartDisposition::RetryPending
        || e.claim
            .as_ref()
            .is_some_and(|c| c.state != RecoveryClaimState::Prepared)
    {
        return Err(invalid("graph is not eligible"));
    }
    let release = request.action == GraphControlAction::Release;
    if !release && e.claim.is_none() {
        return Err(invalid("manual resume requires existing prepared claim"));
    }
    let origin = if !release {
        "manual_resume"
    } else if e.claim.is_some() {
        "manual_release"
    } else {
        "manual_release_without_claim"
    };
    let revision = e
        .control_revision
        .checked_add(1)
        .ok_or_else(|| invalid("graph control revision exhausted"))?;
    let mode = if release {
        "revoked_running"
    } else {
        "manual_running"
    };
    conn.execute(
        "INSERT INTO recovery_graph_control(manifest_id,mode,revision,operation_id,attempt_nonce,revoked,origin) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(manifest_id) DO UPDATE SET mode=excluded.mode,revision=excluded.revision,operation_id=excluded.operation_id,attempt_nonce=excluded.attempt_nonce,revoked=excluded.revoked,origin=excluded.origin",
        params![e.manifest.manifest_id,mode,revision,request.effect.operation_id,request.effect.attempt_nonce,release,origin],
    )?;
    if release {
        if let Some(c) = &e.claim {
            prepared::release(conn, &c.claim_id, "manual graph release")?;
        } else {
            conn.execute(
                "UPDATE restart_components SET disposition='quarantined',reason='ManualReleaseWithoutClaim',revision=revision+1 WHERE restart_id=?1 AND manifest_id=?2",
                params![e.restart_id,e.manifest.manifest_id],
            )?;
        }
    }
    let result =
        control(conn, &e.manifest.manifest_id)?.ok_or_else(|| invalid("missing graph control"))?;
    conn.execute(
        "INSERT INTO recovery_graph_effects VALUES(?1,?2,?3,?4,?5)",
        params![
            request.effect.operation_id,
            request.effect.attempt_nonce,
            e.manifest.manifest_id,
            json,
            encode(&result)?
        ],
    )?;
    Ok(result)
}
