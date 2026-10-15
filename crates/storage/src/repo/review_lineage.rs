//! Explicit lineage edges; ancestry reads fail closed on malformed graphs.

use std::collections::{BTreeMap, HashSet};

use rusqlite::{Connection, params};

use crate::{LineageEdge, StorageError};

const MAX_DEPTH: usize = 64;

/// The caller owns the transaction so edge writes can share snapshot atomicity.
pub fn insert(conn: &Connection, edges: &[LineageEdge]) -> Result<(), StorageError> {
    let mut statement = conn.prepare(
        "INSERT OR IGNORE INTO review_lineage_edges(child_run_id, parent_run_id, created_at_ns) \
         VALUES (?1, ?2, ?3)",
    )?;
    for edge in edges {
        statement.execute(params![
            edge.child_run_id,
            edge.parent_run_id,
            edge.created_at_ns
        ])?;
    }
    Ok(())
}

pub fn for_child(conn: &Connection, run_id: &str) -> Result<Vec<LineageEdge>, StorageError> {
    let mut statement = conn.prepare(
        "SELECT child_run_id, parent_run_id, created_at_ns FROM review_lineage_edges \
         WHERE child_run_id = ?1 ORDER BY parent_run_id",
    )?;
    let rows = statement.query_map([run_id], |row| {
        Ok(LineageEdge {
            child_run_id: row.get(0)?,
            parent_run_id: row.get(1)?,
            created_at_ns: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// All reachable parents, excluding self, in ID order from one consistent snapshot.
pub fn ancestors(conn: &Connection, run_id: &str) -> Result<Vec<String>, StorageError> {
    let transaction = conn.unchecked_transaction()?;
    let mut heights = BTreeMap::new();
    visit(&transaction, run_id, 0, &mut HashSet::new(), &mut heights)?;
    transaction.commit()?;
    heights.remove(run_id);
    Ok(heights.into_keys().collect())
}

fn visit(
    conn: &Connection,
    run_id: &str,
    depth: usize,
    path: &mut HashSet<String>,
    heights: &mut BTreeMap<String, usize>,
) -> Result<usize, StorageError> {
    if depth > MAX_DEPTH {
        return Err(depth_error());
    }
    if path.contains(run_id) {
        return Err(StorageError::Serialization(
            "review lineage cycle detected".into(),
        ));
    }
    // Cache longest remaining paths, not just visited IDs: a later, deeper route
    // to a shared ancestor must still enforce the full path's depth limit.
    if let Some(&height) = heights.get(run_id) {
        return if depth + height > MAX_DEPTH {
            Err(depth_error())
        } else {
            Ok(height)
        };
    }
    path.insert(run_id.to_owned());
    let mut height = 0;
    for edge in for_child(conn, run_id)? {
        height = height.max(1 + visit(conn, &edge.parent_run_id, depth + 1, path, heights)?);
    }
    path.remove(run_id);
    heights.insert(run_id.to_owned(), height);
    Ok(height)
}

fn depth_error() -> StorageError {
    StorageError::Serialization("review lineage exceeds maximum depth of 64".into())
}
