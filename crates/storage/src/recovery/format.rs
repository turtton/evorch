//! Payload-independent recovery inventory. Old recipes are displayed, never decoded.
use super::*;
use restart::{RESTART_FORMAT_VERSION, RestartComponent, RestartPhase};

pub const RECOVERY_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartHeader {
    pub restart_id: String,
    pub capture_id: String,
    pub format_version: u32,
    pub revision: u64,
    pub phase: RestartPhase,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryGraphHeader {
    pub manifest_id: String,
    pub schema_version: u32,
    pub committed: bool,
    pub superseded: bool,
    pub restart: Option<RestartHeader>,
    pub component: Option<RestartComponent>,
    /// Current relational thread/root bindings, not values learned from a recipe.
    pub bindings: Vec<(String, Option<String>)>,
}
impl RecoveryGraphHeader {
    pub fn is_current(&self) -> bool {
        self.schema_version == RECOVERY_SCHEMA_VERSION
            && self
                .restart
                .as_ref()
                .is_none_or(|r| r.format_version == RESTART_FORMAT_VERSION)
    }
}

impl Database {
    pub fn restart_headers(&self) -> Result<Vec<RestartHeader>, StorageError> {
        restart_headers(&self.conn)
    }
    pub fn restart_header_for_manifest(
        &self,
        id: &str,
    ) -> Result<Option<RestartHeader>, StorageError> {
        let restart: Option<String> = self
            .conn
            .query_row(
                "SELECT restart_id FROM restart_components WHERE manifest_id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        restart
            .map(|id| {
                restart_header(&self.conn, &id)?.ok_or_else(|| invalid("missing restart header"))
            })
            .transpose()
    }
    pub fn recovery_graph_headers(&self) -> Result<Vec<RecoveryGraphHeader>, StorageError> {
        let ids = self
            .conn
            .prepare("SELECT manifest_id FROM recovery_manifests ORDER BY manifest_id")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.iter().map(|id| graph_header(&self.conn, id)).collect()
    }
}
fn restart_headers(conn: &Connection) -> Result<Vec<RestartHeader>, StorageError> {
    let ids = conn
        .prepare("SELECT restart_id FROM restart_transactions ORDER BY restart_id")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    ids.iter()
        .map(|id| restart_header(conn, id)?.ok_or_else(|| invalid("missing restart header")))
        .collect()
}
fn restart_header(conn: &Connection, id: &str) -> Result<Option<RestartHeader>, StorageError> {
    let row = conn.query_row("SELECT capture_id,format_version,revision,phase FROM restart_transactions WHERE restart_id=?1", [id],
        |r| Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?, r.get::<_, String>(3)?))).optional()?;
    row.map(|(capture_id, format_version, revision, phase)| {
        Ok(RestartHeader {
            restart_id: id.into(),
            capture_id,
            format_version,
            revision,
            phase: serde_json::from_value(serde_json::Value::String(phase))
                .map_err(|_| invalid("invalid restart phase"))?,
        })
    })
    .transpose()
}
fn graph_header(conn: &Connection, id: &str) -> Result<RecoveryGraphHeader, StorageError> {
    let (schema_version, committed, superseded) = conn.query_row(
        "SELECT schema_version,committed,superseded FROM recovery_manifests WHERE manifest_id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let component = conn.query_row(
        "SELECT restart_id,disposition,claim_id,reason,revision FROM restart_components WHERE manifest_id=?1", [id],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?;
    let (restart, component) = match component {
        None => (None, None),
        Some((restart_id, disposition, claim_id, reason, revision)) => (
            Some(
                restart_header(conn, &restart_id)?
                    .ok_or_else(|| invalid("missing restart header"))?,
            ),
            Some(RestartComponent {
                manifest_id: id.into(),
                disposition: serde_json::from_value(serde_json::Value::String(disposition))
                    .map_err(|_| invalid("invalid component disposition"))?,
                claim_id,
                reason,
                revision,
            }),
        ),
    };
    let bindings = conn.prepare(
        "SELECT DISTINCT t.thread_id,t.root_run_id FROM threads t WHERE t.root_run_id IN (SELECT run_id FROM recovery_members WHERE manifest_id=?1 UNION SELECT root_run_id FROM restart_expected_members WHERE manifest_id=?1) OR t.thread_id IN (SELECT tr.thread_id FROM thread_runs tr WHERE tr.run_id IN (SELECT run_id FROM recovery_members WHERE manifest_id=?1 UNION SELECT root_run_id FROM restart_expected_members WHERE manifest_id=?1) OR tr.root_run_id IN (SELECT run_id FROM recovery_members WHERE manifest_id=?1 UNION SELECT root_run_id FROM restart_expected_members WHERE manifest_id=?1)) ORDER BY t.thread_id"
    )?.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
    Ok(RecoveryGraphHeader {
        manifest_id: id.into(),
        schema_version,
        committed,
        superseded,
        restart,
        component,
        bindings,
    })
}

/// Explicitly release only a quarantined unsupported graph. Never touch current
/// intents/incarnations/Stop or external resources. The caller must show all scratch
/// as retained: relational membership is not evidence that an external effect is ours.
pub(super) fn discard_legacy(
    conn: &Connection,
    expected: &RecoveryGraphHeader,
) -> Result<(), StorageError> {
    let current = graph_header(conn, &expected.manifest_id)?;
    if current.is_current()
        || current.schema_version != 1
        || current
            .restart
            .as_ref()
            .is_some_and(|r| r.format_version != 1)
    {
        return Err(invalid("not a legacy recovery graph"));
    }
    if let Some(r) = &current.restart {
        if !matches!(
            r.phase,
            RestartPhase::Invalidated
                | RestartPhase::Aborted
                | RestartPhase::Superseded
                | RestartPhase::Completed
        ) || current
            .component
            .as_ref()
            .is_some_and(|c| c.disposition == restart::RestartDisposition::RetryPending)
        {
            return Err(invalid("legacy graph must be quarantined first"));
        }
    }
    let mut after = expected.clone();
    after.superseded = true;
    if let Some(c) = &mut after.component {
        c.disposition = restart::RestartDisposition::Released;
        c.reason = Some("legacy graph explicitly discarded".into());
        c.revision = c
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("component revision exhausted"))?;
    }
    if let Some(r) = &mut after.restart {
        r.revision = r
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("restart revision exhausted"))?;
    }
    if current == after {
        return Ok(());
    }
    if current != *expected {
        return Err(invalid("legacy graph revision/binding conflict"));
    }
    // Activated old claims can be released only after an explicit durable Stop.
    // Inspect current intents, never the old recovery recipe; do not alter them.
    let unsafe_activated: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM recovery_claims c WHERE c.manifest_id=?1 AND c.state='activated' AND ((SELECT count(*) FROM recovery_claim_members m WHERE m.claim_id=c.claim_id)=0 OR EXISTS(SELECT 1 FROM recovery_claim_members m LEFT JOIN resume_intents i USING(run_id) WHERE m.claim_id=c.claim_id AND (i.run_id IS NULL OR i.incarnation!=m.new_incarnation OR i.revision<=m.reserved_revision OR CASE WHEN json_valid(i.payload) THEN (json_extract(i.payload,'$.active') IS NOT 0 OR coalesce(json_extract(i.payload,'$.stop_reason'),'') NOT IN ('operator_stopped','cancelled')) ELSE 1 END))))", [&expected.manifest_id], |r| r.get(0))?;
    if unsafe_activated {
        return Err(invalid(
            "legacy activated graph requires durable Stop before discard",
        ));
    }
    conn.execute("UPDATE recovery_claims SET state='released',release_reason='legacy graph explicitly discarded' WHERE manifest_id=?1 AND state IN ('prepared','activated')", [&expected.manifest_id])?;
    conn.execute(
        "UPDATE recovery_manifests SET superseded=1 WHERE manifest_id=?1",
        [&expected.manifest_id],
    )?;
    if let Some(r) = &expected.restart {
        conn.execute("UPDATE restart_components SET disposition='released',reason='legacy graph explicitly discarded',revision=revision+1 WHERE manifest_id=?1", [&expected.manifest_id])?;
        conn.execute(
            "UPDATE restart_transactions SET revision=revision+1 WHERE restart_id=?1",
            [&r.restart_id],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn old_graph(db: &Database, restart: bool, state: &str) {
        db.conn
            .execute(
                "INSERT INTO recovery_manifests VALUES('legacy',1,1,0,1,0,'broken edges')",
                [],
            )
            .unwrap();
        db.conn.execute("INSERT INTO recovery_members VALUES('legacy','run','old',0,NULL,1,'broken recipe')", []).unwrap();
        db.conn
            .execute(
                "INSERT INTO recovery_claims VALUES('claim','legacy',?1,NULL)",
                [state],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO recovery_claim_members VALUES('claim','run','old',0,'new')",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO resume_intents(run_id,incarnation,payload) VALUES('run','new',?1)",
                [r#"{"run_id":"run","incarnation":"new","active":true,"stop_reason":null}"#],
            )
            .unwrap();
        if restart {
            db.conn.execute("INSERT INTO restart_transactions(restart_id,capture_id,revision,phase,metadata_json,old_builtin_tool_names_json) VALUES('old','capture',0,'invalidated','broken metadata','[]')", []).unwrap();
            db.conn.execute("INSERT INTO restart_components VALUES('old','legacy','quarantined','claim','cold boot has no inherited proof',0)", []).unwrap();
            db.conn.execute("INSERT INTO restart_expected_members VALUES('old','run','run','broken member','legacy')", []).unwrap();
        }
    }
    fn discard(db: &Database, header: &RecoveryGraphHeader) -> Result<(), StorageError> {
        super::super::write(&db.conn, Write::DiscardLegacy(header.clone())).map(|_| ())
    }
    #[test]
    fn legacy_payload_is_row_local_and_explicit_discard_is_cas_idempotent() {
        for restart in [false, true] {
            let db = Database::open_in_memory().unwrap();
            old_graph(&db, restart, "prepared");
            let headers = db.recovery_graph_headers().unwrap();
            assert_eq!(headers.len(), 1);
            let h = &headers[0];
            assert!(!h.is_current());
            assert_eq!(
                db.recovery_manifests().unwrap()[0].record,
                Err(RecoveryProblem::UnsupportedFormat)
            );
            assert!(db.owned_graph_snapshots().unwrap().is_empty());
            assert!(db.restart_transaction("old").is_err() || !restart);
            discard(&db, h).unwrap();
            discard(&db, h).unwrap();
            assert!(matches!(
                db.recovery_claims().unwrap()[0].state,
                RecoveryClaimState::Released { .. }
            ));
            let after = db.resume_intent("run").unwrap().unwrap();
            assert_eq!(after.incarnation, "new");
            assert!(after.payload.contains("\"active\":true"));
            if restart {
                db.conn.execute("UPDATE restart_transactions SET revision=revision+1 WHERE restart_id='old'", []).unwrap();
                assert!(discard(&db, h).is_err());
            }
        }
    }
    #[test]
    fn activated_legacy_claim_requires_current_durable_stop_and_never_rewrites_it() {
        let db = Database::open_in_memory().unwrap();
        old_graph(&db, true, "activated");
        let h = db.recovery_graph_headers().unwrap().remove(0);
        assert!(discard(&db, &h).is_err());
        db.conn.execute("UPDATE resume_intents SET payload=?1 WHERE run_id='run'", [r#"{"run_id":"run","incarnation":"new","active":false,"stop_reason":"operator_stopped"}"#]).unwrap();
        let stopped = db.resume_intent("run").unwrap();
        discard(&db, &h).unwrap();
        assert_eq!(db.resume_intent("run").unwrap(), stopped);
        discard(&db, &h).unwrap();
    }
    #[test]
    fn legacy_header_checks_current_binding_of_historical_child_member() {
        let db = Database::open_in_memory().unwrap();
        old_graph(&db, false, "prepared");
        db.conn.execute("INSERT INTO threads(thread_id,project_root,root_run_id,created_at_ns,updated_at_ns,source) VALUES('thread','/p','current-root',1,1,'host')", []).unwrap();
        db.conn
            .execute(
                "INSERT INTO thread_runs VALUES('run','thread','historical-root',1)",
                [],
            )
            .unwrap();
        let header = db.recovery_graph_headers().unwrap().remove(0);
        assert_eq!(
            header.bindings,
            vec![("thread".into(), Some("current-root".into()))]
        );
        db.conn
            .execute(
                "UPDATE threads SET root_run_id='new-current-root' WHERE thread_id='thread'",
                [],
            )
            .unwrap();
        assert!(
            discard(&db, &header).is_err(),
            "changed binding must invalidate a management CAS"
        );
        assert!(matches!(
            db.recovery_claims().unwrap()[0].state,
            RecoveryClaimState::Prepared
        ));
    }

    #[test]
    fn mixed_versions_are_unsupported_before_any_recipe_decode() {
        let db = Database::open_in_memory().unwrap();
        old_graph(&db, true, "prepared");
        db.conn
            .execute("UPDATE recovery_manifests SET schema_version=2", [])
            .unwrap();
        assert!(!db.recovery_graph_headers().unwrap()[0].is_current());
        assert_eq!(
            db.recovery_manifests().unwrap()[0].record,
            Err(RecoveryProblem::UnsupportedFormat)
        );
    }
}
