//! Header-only guard for a legacy proof derived outside the writer.
//!
//! Snapshot revisions cover canonical bytes; bindings and fork/lineage rows
//! have no such revision, so their exact identities must be checked separately.
use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension};

use crate::StorageError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LegacyDependencies {
    root: String,
    headers: BTreeMap<String, Option<String>>,
}

impl LegacyDependencies {
    pub(crate) fn capture(conn: &Connection, root: &str) -> Result<Option<Self>, StorageError> {
        let mut pending = BTreeSet::from([root.to_owned()]);
        let mut headers = BTreeMap::new();
        let mut edges = BTreeMap::new();
        while let Some(run) = pending.pop_first() {
            if headers.contains_key(&run) {
                continue;
            }
            if headers.len() >= crate::threads::MAX_RUN_LINKS as usize {
                return Ok(None);
            }
            let row: Option<(Option<String>, Option<String>, String)> = conn
                .query_row(
                    "SELECT c.parent_run_id,t.fork_point_run_id, \
                 json_array(c.snapshot_revision,c.parent_run_id,r.source_revision, \
                   b.thread_id,b.root_run_id,b.depth,t.root_run_id,t.parent_thread_id, \
                   t.lineage_kind,t.fork_point_run_id,t.fork_point_context_len, \
                   t.escalation_source_run_id,p.snapshot_revision,p.snapshot_sha256, \
                   p.format_version,p.status,p.reason_code,p.evidence_json, \
                   EXISTS(SELECT 1 FROM review_lineage_edges e WHERE e.child_run_id=c.run_id)) \
                 FROM run_contexts c \
                 LEFT JOIN thread_index_runs r ON r.run_id=c.run_id \
                 LEFT JOIN thread_runs b ON b.run_id=c.run_id \
                 LEFT JOIN threads t ON t.thread_id=b.thread_id \
                 LEFT JOIN run_context_provenance p ON p.run_id=c.run_id \
                 WHERE c.run_id=?1",
                    [&run],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            if let Some((parent, fork, header)) = row {
                let dependencies: BTreeSet<_> = [parent, fork].into_iter().flatten().collect();
                for dependency in &dependencies {
                    if !headers.contains_key(dependency) {
                        pending.insert(dependency.clone());
                    }
                }
                edges.insert(run.clone(), dependencies);
                headers.insert(run, Some(header));
            } else {
                edges.insert(run.clone(), BTreeSet::new());
                headers.insert(run, None);
            }
        }
        // Remove leaves iteratively: a remaining component contains a cycle.
        while !edges.is_empty() {
            let leaves: BTreeSet<_> = edges
                .iter()
                .filter(|(_, deps)| deps.is_empty())
                .map(|(run, _)| run.clone())
                .collect();
            if leaves.is_empty() {
                return Ok(None);
            }
            edges.retain(|run, _| !leaves.contains(run));
            for deps in edges.values_mut() {
                deps.retain(|run| !leaves.contains(run));
            }
        }
        Ok(Some(Self {
            root: root.to_owned(),
            headers,
        }))
    }

    pub(crate) fn unchanged(&self, conn: &Connection) -> Result<bool, StorageError> {
        Ok(Self::capture(conn, &self.root)?.as_ref() == Some(self))
    }
}
