//! Read-only Q2 snapshots and supplemental scratch users. These facts grant no authority.
use super::*;
use crate::StorageConfig;
use graph_control::{GraphControlCas, GraphControlRecord};
use graph_progress::{GraphDisposalProgressKey, GraphThreadProgress};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedGraphScratchUsers {
    pub shared_threads: BTreeSet<String>,
    pub unknown: bool,
}
#[derive(Debug, Clone)]
pub struct OwnedGraphSnapshot {
    pub cas: GraphControlCas,
    pub adopted_epoch: Option<u64>,
    pub control: Option<GraphControlRecord>,
    pub progress: Vec<GraphThreadProgress>,
}
impl Database {
    /// Open without migrations, schema creation, or write pragmas. One read snapshot
    /// covers the whole callback; a malformed row fails the entire observation.
    pub fn read_owned_graphs<T>(
        config: &StorageConfig,
        read: impl FnOnce(&Database) -> Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        let conn = Connection::open_with_flags(
            &config.db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let db = Database { conn };
        let snapshot = db.conn.unchecked_transaction()?;
        let result = read(&db);
        drop(snapshot);
        result
    }
    pub fn owned_graph_snapshots(&self) -> Result<Vec<OwnedGraphSnapshot>, StorageError> {
        let mut result = Vec::new();
        for header in self.restart_headers()? {
            if header.format_version != restart::RESTART_FORMAT_VERSION {
                continue;
            }
            let tx = self
                .restart_transaction(&header.restart_id)?
                .ok_or_else(|| invalid("restart disappeared"))?;
            for component in &tx.components {
                let schema: u32 = self.conn.query_row(
                    "SELECT schema_version FROM recovery_manifests WHERE manifest_id=?1",
                    [&component.manifest_id],
                    |r| r.get(0),
                )?;
                if schema != RECOVERY_SCHEMA_VERSION {
                    continue;
                }
                let manifest = load(&self.conn, &component.manifest_id)
                    .map_err(|_| invalid("unreadable owned graph"))?;
                let assigned: Vec<String> = self.conn.prepare(
                    "SELECT run_id FROM restart_expected_members WHERE restart_id=?1 AND manifest_id=?2 ORDER BY run_id"
                )?.query_map(params![tx.metadata.restart_id, component.manifest_id], |r| r.get(0))?
                    .collect::<Result<_, _>>()?;
                let mut captured = manifest
                    .members
                    .iter()
                    .map(|m| m.run_id.clone())
                    .collect::<Vec<_>>();
                captured.sort();
                if captured != assigned {
                    return Err(invalid("incomplete owned graph"));
                }
                let membership = tx
                    .expected
                    .iter()
                    .filter(|m| assigned.contains(&m.run_id))
                    .cloned()
                    .collect::<Vec<_>>();
                if membership.len() != assigned.len() {
                    return Err(invalid("incomplete owned inventory"));
                }
                let ids: Vec<String> = self.conn.prepare(
                    "SELECT claim_id FROM recovery_claims WHERE manifest_id=?1 ORDER BY claim_id"
                )?.query_map([&component.manifest_id], |r| r.get(0))?.collect::<Result<_, _>>()?;
                let claim = match ids.as_slice() {
                    [] => None,
                    [id] => Some(claim(&self.conn, id)?),
                    _ => return Err(invalid("ambiguous owned claim")),
                };
                let control = self.recovery_graph_control(&component.manifest_id)?;
                let keys: Vec<String> = self.conn.prepare(
                    "SELECT thread_id FROM recovery_graph_thread_progress WHERE manifest_id=?1 AND restart_id=?2 AND capture_id=?3 ORDER BY thread_id"
                )?.query_map(params![component.manifest_id,tx.metadata.restart_id,tx.metadata.capture_id], |r| r.get(0))?.collect::<Result<_, _>>()?;
                let progress = keys
                    .into_iter()
                    .map(|thread_id| {
                        self.recovery_graph_thread_progress(&GraphDisposalProgressKey {
                            manifest_id: component.manifest_id.clone(),
                            restart_id: tx.metadata.restart_id.clone(),
                            capture_id: tx.metadata.capture_id.clone(),
                            thread_id,
                        })?
                        .ok_or_else(|| invalid("missing owned progress"))
                    })
                    .collect::<Result<_, _>>()?;
                result.push(OwnedGraphSnapshot {
                    cas: GraphControlCas {
                        restart_id: tx.metadata.restart_id.clone(),
                        capture_id: tx.metadata.capture_id.clone(),
                        successor_epoch: tx.metadata.successor_epoch,
                        phase: tx.phase,
                        component: component.clone(),
                        membership,
                        manifest,
                        claim,
                        control_revision: control.as_ref().map_or(0, |c| c.revision),
                        control_effect: control.as_ref().map(|c| c.effect.clone()),
                    },
                    adopted_epoch: tx.adopted_epoch,
                    control,
                    progress,
                });
            }
        }
        Ok(result)
    }
    /// Other graphs (including cold/Prepared graphs) and unrelated chat roots
    /// sharing a thread keep both its planned receipt and scratch alive.
    pub fn owned_graph_shared_threads(
        &self,
        target: &GraphControlCas,
    ) -> Result<BTreeSet<String>, StorageError> {
        let users = self.owned_graph_scratch_users(target)?;
        if users.unknown {
            Ok(target
                .membership
                .iter()
                .filter_map(|m| m.thread_id.clone())
                .collect())
        } else {
            Ok(users.shared_threads)
        }
    }
    pub fn owned_graph_scratch_users(
        &self,
        target: &GraphControlCas,
    ) -> Result<OwnedGraphScratchUsers, StorageError> {
        // A missing DTO thread is not proof of a threadless member. Check both
        // current and historical relational root bindings before dropping it.
        for member in target.membership.iter().filter(|m| m.thread_id.is_none()) {
            let bound: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM threads WHERE root_run_id IN (?1,?2)) OR EXISTS(SELECT 1 FROM thread_runs WHERE run_id IN (?1,?2) OR root_run_id IN (?1,?2))",
                params![member.run_id, member.root_run_id], |r| r.get(0))?;
            if bound {
                return Ok(OwnedGraphScratchUsers {
                    shared_threads: BTreeSet::new(),
                    unknown: true,
                });
            }
        }
        let threads = target
            .membership
            .iter()
            .filter_map(|m| m.thread_id.clone())
            .collect::<BTreeSet<_>>();
        let mut shared = BTreeSet::new();
        // Inspect other graphs relationally, including old/corrupt recipes. Unknown
        // correspondence retains every target scratch instead of blocking disposal.
        let unknown: bool = self.conn.query_row("SELECT EXISTS(SELECT 1 FROM recovery_manifests g WHERE g.manifest_id!=?1 AND g.superseded=0 AND (g.member_count<1 OR g.member_count!=(SELECT count(*) FROM recovery_members m WHERE m.manifest_id=g.manifest_id))) OR EXISTS(SELECT 1 FROM recovery_manifests g WHERE g.manifest_id!=?1 AND g.superseded=0 AND NOT EXISTS(SELECT 1 FROM restart_components c WHERE c.manifest_id=g.manifest_id) AND EXISTS(SELECT 1 FROM recovery_members m WHERE m.manifest_id=g.manifest_id AND NOT EXISTS(SELECT 1 FROM thread_runs r WHERE r.run_id=m.run_id OR r.root_run_id=m.run_id) AND NOT EXISTS(SELECT 1 FROM threads t WHERE t.root_run_id=m.run_id)))", [&target.manifest.manifest_id], |r| r.get(0))?;
        if unknown {
            return Ok(OwnedGraphScratchUsers {
                shared_threads: BTreeSet::new(),
                unknown: true,
            });
        }
        for thread in &threads {
            let other: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM recovery_members m JOIN recovery_manifests g USING(manifest_id) WHERE g.manifest_id!=?1 AND g.superseded=0 AND m.run_id IN (SELECT root_run_id FROM threads WHERE thread_id=?2 UNION SELECT root_run_id FROM thread_runs WHERE thread_id=?2 UNION SELECT run_id FROM thread_runs WHERE thread_id=?2)) OR EXISTS(SELECT 1 FROM restart_expected_members e JOIN restart_transactions t USING(restart_id) WHERE t.restart_id!=?3 AND (e.root_run_id IN (SELECT root_run_id FROM threads WHERE thread_id=?2 UNION SELECT root_run_id FROM thread_runs WHERE thread_id=?2) OR e.run_id IN (SELECT run_id FROM thread_runs WHERE thread_id=?2)))",
                params![target.manifest.manifest_id, thread, target.restart_id], |r| r.get(0))?;
            if other {
                shared.insert(thread.clone());
            }
        }
        let captured = target
            .membership
            .iter()
            .map(|m| m.run_id.as_str())
            .collect::<BTreeSet<_>>();
        for thread in threads {
            let roots: Vec<String> = self.conn.prepare(
                "SELECT root_run_id FROM threads WHERE thread_id=?1 AND root_run_id IS NOT NULL UNION SELECT root_run_id FROM thread_runs WHERE thread_id=?1"
            )?.query_map([&thread], |r|r.get(0))?.collect::<Result<_, _>>()?;
            if roots.iter().any(|root| !captured.contains(root.as_str())) {
                shared.insert(thread);
            }
        }
        Ok(OwnedGraphScratchUsers {
            shared_threads: shared,
            unknown: false,
        })
    }
}

#[cfg(test)]
mod scratch_membership_tests {
    use super::*;
    use crate::recovery::{
        RecoveryManifestRecord,
        graph_control::GraphControlCas,
        restart::{RestartComponent, RestartDisposition, RestartExpectedMember, RestartPhase},
    };
    fn target() -> GraphControlCas {
        GraphControlCas {
            restart_id: "tx".into(),
            capture_id: "capture".into(),
            successor_epoch: 2,
            phase: RestartPhase::Adopted,
            component: RestartComponent {
                manifest_id: "graph".into(),
                disposition: RestartDisposition::Quarantined,
                claim_id: None,
                reason: None,
                revision: 0,
            },
            membership: vec![RestartExpectedMember {
                run_id: "child".into(),
                root_run_id: "root".into(),
                thread_id: None,
                incarnation: "inc".into(),
                intent_revision: 0,
                snapshot: None,
                lane_id: None,
            }],
            manifest: RecoveryManifestRecord {
                manifest_id: "graph".into(),
                schema_version: 2,
                committed: true,
                superseded: false,
                edges: vec![],
                members: vec![],
            },
            claim: None,
            control_revision: 0,
            control_effect: None,
        }
    }
    #[test]
    fn absent_thread_requires_both_current_and_historical_relational_absence() {
        for source in ["none", "current", "historical", "inconsistent_child"] {
            let db = Database::open_in_memory().unwrap();
            if source != "none" {
                let root = if source == "current" {
                    "root"
                } else {
                    "unrelated-root"
                };
                db.conn.execute("INSERT INTO threads(thread_id,project_root,root_run_id,created_at_ns,updated_at_ns,source) VALUES ('thread','/p',?1,1,1,'host')", [root]).unwrap();
                if source != "current" {
                    let (run, root) = if source == "historical" {
                        ("other", "root")
                    } else {
                        ("child", "unrelated-root")
                    };
                    db.conn.execute("INSERT INTO thread_runs(run_id,thread_id,root_run_id,depth) VALUES (?1,'thread',?2,1)", params![run,root]).unwrap();
                }
            }
            let users = db.owned_graph_scratch_users(&target()).unwrap();
            assert_eq!(users.unknown, source != "none", "{source}");
            assert!(users.shared_threads.is_empty());
        }
    }
    fn bound_thread(db: &Database) {
        db.conn.execute("INSERT INTO threads(thread_id,project_root,root_run_id,created_at_ns,updated_at_ns,source) VALUES ('thread','/p','root',1,1,'host')", []).unwrap();
        for (run, depth) in [("root", 0), ("child", 1)] {
            db.conn.execute("INSERT INTO thread_runs(run_id,thread_id,root_run_id,depth) VALUES (?1,'thread','root',?2)", params![run, depth]).unwrap();
        }
    }
    fn target_of(run: &str) -> GraphControlCas {
        let mut target = target();
        target.membership[0].run_id = run.into();
        target.membership[0].root_run_id = "root".into();
        target.membership[0].thread_id = Some("thread".into());
        target
    }
    fn standalone(db: &Database, manifest: &str, run: &str, superseded: bool) {
        db.conn
            .execute(
                "INSERT INTO recovery_manifests VALUES(?1,1,1,?2,1,0,'[]')",
                params![manifest, superseded],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO recovery_members VALUES(?1,?2,'old',0,NULL,1,'{}')",
                params![manifest, run],
            )
            .unwrap();
    }
    #[test]
    fn a_standalone_graph_sharing_a_thread_through_a_child_or_root_keeps_its_scratch() {
        // (the target's run, the old standalone graph's run, expected shared)
        for (mine, theirs) in [("root", "child"), ("child", "root"), ("root", "root")] {
            let db = Database::open_in_memory().unwrap();
            bound_thread(&db);
            standalone(&db, "old", theirs, false);
            let users = db.owned_graph_scratch_users(&target_of(mine)).unwrap();
            assert!(!users.unknown, "{mine}/{theirs}");
            assert_eq!(
                users.shared_threads,
                BTreeSet::from(["thread".to_owned()]),
                "{mine}/{theirs}"
            );
        }
    }
    #[test]
    fn an_old_member_that_cannot_be_placed_retains_the_scratch_but_a_finished_graph_does_not() {
        let db = Database::open_in_memory().unwrap();
        bound_thread(&db);
        standalone(&db, "old", "ghost", false);
        let users = db.owned_graph_scratch_users(&target_of("root")).unwrap();
        assert!(users.unknown && users.shared_threads.is_empty());
        // The logical result is untouched: only the physical step is retained.
        assert_eq!(
            db.owned_graph_shared_threads(&target_of("root")).unwrap(),
            BTreeSet::from(["thread".to_owned()])
        );
        let db = Database::open_in_memory().unwrap();
        bound_thread(&db);
        standalone(&db, "old", "ghost", true);
        let users = db.owned_graph_scratch_users(&target_of("root")).unwrap();
        assert!(!users.unknown && users.shared_threads.is_empty());
    }
}
