//! Read-only Q2 snapshots and supplemental scratch users. These facts grant no authority.
use super::*;
use crate::StorageConfig;
use graph_control::{GraphControlCas, GraphControlRecord};
use graph_progress::{GraphDisposalProgressKey, GraphThreadProgress};
use std::collections::BTreeSet;

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
        for tx in self.restart_transactions()? {
            for component in &tx.components {
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
                let keys: Vec<(String,u64)> = self.conn.prepare(
                    "SELECT thread_id,owner_generation FROM recovery_graph_thread_progress WHERE manifest_id=?1 AND restart_id=?2 AND capture_id=?3 ORDER BY thread_id,owner_generation"
                )?.query_map(params![component.manifest_id,tx.metadata.restart_id,tx.metadata.capture_id], |r| Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<_, _>>()?;
                let progress = keys
                    .into_iter()
                    .map(|(thread_id, owner_generation)| {
                        self.recovery_graph_thread_progress(&GraphDisposalProgressKey {
                            manifest_id: component.manifest_id.clone(),
                            restart_id: tx.metadata.restart_id.clone(),
                            capture_id: tx.metadata.capture_id.clone(),
                            thread_id,
                            owner_generation,
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
        let threads = target
            .membership
            .iter()
            .filter_map(|m| m.thread_id.clone())
            .collect::<BTreeSet<_>>();
        let mut shared = BTreeSet::new();
        for graph in self.owned_graph_snapshots()? {
            if graph.cas.restart_id == target.restart_id
                && graph.cas.manifest.manifest_id == target.manifest.manifest_id
            {
                continue;
            }
            if graph.control.as_ref().is_some_and(|c| c.revoked) {
                continue;
            }
            for member in graph.cas.membership {
                if let Some(thread) = member.thread_id.filter(|t| threads.contains(t)) {
                    shared.insert(thread);
                }
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
        Ok(shared)
    }
}
