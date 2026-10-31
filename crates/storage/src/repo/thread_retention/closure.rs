//! Family closure: the threads and runs that must be deleted together.
//!
//! A family is the connected set of threads and runs under every relation
//! that ties their rows together (lineage, bindings, delegation, usage
//! parents, escalation). The search is a worklist kept in `retention_staging`
//! so it can stop and resume across startups; each step reads only indexed
//! rows and counts them against a budget.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use super::candidates::family_blocked;
use crate::StorageError;
use crate::migrations::backfill::backfill_pending;
use crate::thread_retention::{
    BlockReason, ExploreBudget, FamilyState, RetentionInput, SidebarThread, ThreadRetentionPolicy,
};

/// A member reached from another member, or a deleted identity in the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Neighbor {
    Thread(String),
    Run(String),
    /// Already tombstoned: the family overlaps a confirmed deletion.
    Gone(String),
}

fn thread_neighbor(conn: &Connection, thread_id: String) -> Result<Neighbor, StorageError> {
    let gone: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM thread_tombstones WHERE thread_id = ?1)",
        [&thread_id],
        |row| row.get(0),
    )?;
    Ok(if gone {
        Neighbor::Gone(thread_id)
    } else {
        Neighbor::Thread(thread_id)
    })
}

fn run_neighbor(conn: &Connection, run_id: String) -> Result<Neighbor, StorageError> {
    let gone: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id = ?1)",
        [&run_id],
        |row| row.get(0),
    )?;
    Ok(if gone {
        Neighbor::Gone(run_id)
    } else {
        Neighbor::Run(run_id)
    })
}

/// Run `sql` with one parameter and map each single-column row through `map`.
/// Counts the statement and every row it returns as edges.
fn column(
    conn: &Connection,
    sql: &str,
    key: &str,
    edges: &mut u64,
    mut visit: impl FnMut(String) -> Result<(), StorageError>,
) -> Result<(), StorageError> {
    *edges += 1;
    let mut statement = conn.prepare_cached(sql)?;
    let mut rows = statement.query([key])?;
    while let Some(row) = rows.next()? {
        *edges += 1;
        if let Some(value) = row.get::<_, Option<String>>(0)? {
            visit(value)?;
        }
    }
    Ok(())
}

/// Every thread and run one hop from `thread_id`.
pub(super) fn thread_neighbors(
    conn: &Connection,
    thread_id: &str,
    edges: &mut u64,
) -> Result<Vec<Neighbor>, StorageError> {
    let mut out = Vec::new();
    // Its own row: parent thread, recorded root run, escalation source run.
    *edges += 1;
    let own = conn
        .query_row(
            "SELECT parent_thread_id, root_run_id, escalation_source_run_id \
             FROM threads WHERE thread_id = ?1",
            [thread_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    if let Some((parent, root, source)) = own {
        if let Some(parent) = parent {
            out.push(thread_neighbor(conn, parent)?);
        }
        for run in [root, source].into_iter().flatten() {
            out.push(run_neighbor(conn, run)?);
        }
    }
    let mut found = Vec::new();
    column(
        conn,
        "SELECT thread_id FROM threads WHERE parent_thread_id = ?1",
        thread_id,
        edges,
        |child| {
            found.push(Neighbor::Thread(child));
            Ok(())
        },
    )?;
    let mut runs = Vec::new();
    column(
        conn,
        "SELECT run_id FROM thread_runs WHERE thread_id = ?1",
        thread_id,
        edges,
        |run| {
            runs.push(run);
            Ok(())
        },
    )?;
    // Conversation roots named for the thread, the same predicate as `runs_of_thread`.
    column(
        conn,
        "SELECT run_id FROM run_contexts \
         WHERE parent_run_id IS NULL AND substr(name, 1, 5) = 'chat:' \
         AND instr(substr(name, 6), ':') > 1 \
         AND substr(name, 6 + instr(substr(name, 6), ':')) = ?1",
        thread_id,
        edges,
        |run| {
            runs.push(run);
            Ok(())
        },
    )?;
    for neighbor in found {
        if let Neighbor::Thread(id) = neighbor {
            out.push(thread_neighbor(conn, id)?);
        }
    }
    for run in runs {
        out.push(run_neighbor(conn, run)?);
    }
    Ok(out)
}

/// Every thread and run one hop from `run_id`.
pub(super) fn run_neighbors(
    conn: &Connection,
    run_id: &str,
    edges: &mut u64,
) -> Result<Vec<Neighbor>, StorageError> {
    let mut threads = Vec::new();
    let mut runs = Vec::new();
    for sql in [
        "SELECT thread_id FROM thread_runs WHERE run_id = ?1",
        "SELECT thread_id FROM threads WHERE escalation_source_run_id = ?1",
        "SELECT thread_id FROM threads WHERE root_run_id = ?1",
    ] {
        column(conn, sql, run_id, edges, |thread| {
            threads.push(thread);
            Ok(())
        })?;
    }
    for sql in [
        "SELECT parent_run_id FROM run_contexts WHERE run_id = ?1",
        "SELECT run_id FROM run_contexts WHERE parent_run_id = ?1",
        "SELECT DISTINCT run_id FROM usage_requests \
         WHERE parent_run_id = ?1 AND run_id IS NOT NULL",
        "SELECT DISTINCT parent_run_id FROM usage_requests \
         WHERE run_id = ?1 AND parent_run_id IS NOT NULL",
    ] {
        column(conn, sql, run_id, edges, |run| {
            runs.push(run);
            Ok(())
        })?;
    }
    let mut out = Vec::new();
    for thread in threads {
        out.push(thread_neighbor(conn, thread)?);
    }
    for run in runs {
        out.push(run_neighbor(conn, run)?);
    }
    Ok(out)
}

/// Adds a member to a family's staging and its worklist. When the member already
/// belongs to another family the two are merged under the smaller key. Returns
/// the key the family now has and whether the member was new to it.
pub(super) fn add_member(
    conn: &Connection,
    family_key: &str,
    kind: &str,
    id: &str,
) -> Result<(String, bool), StorageError> {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO retention_staging(family_key, kind, id) VALUES (?1, ?2, ?3)",
        params![family_key, kind, id],
    )?;
    if inserted == 0 {
        return Ok((family_key.to_owned(), false));
    }
    conn.execute(
        "INSERT OR IGNORE INTO retention_staging(family_key, kind, id) VALUES (?1, ?2, ?3)",
        params![family_key, format!("frontier_{kind}"), id],
    )?;
    let other: Option<String> = conn
        .query_row(
            "SELECT family_key FROM retention_staging \
             WHERE kind = ?1 AND id = ?2 AND family_key <> ?3 LIMIT 1",
            params![kind, id, family_key],
            |row| row.get(0),
        )
        .optional()?;
    let key = match other {
        Some(other) => merge(conn, family_key, &other)?,
        None => family_key.to_owned(),
    };
    Ok((key, true))
}

/// Folds the family with the larger key into the one with the smaller key.
fn merge(conn: &Connection, a: &str, b: &str) -> Result<String, StorageError> {
    let (keep, drop) = if a <= b { (a, b) } else { (b, a) };
    conn.execute(
        "UPDATE OR IGNORE retention_staging SET family_key = ?1 WHERE family_key = ?2",
        params![keep, drop],
    )?;
    conn.execute(
        "UPDATE retention_staging_state SET explored_edges = explored_edges + \
         COALESCE((SELECT explored_edges FROM retention_staging_state WHERE family_key = ?2), 0) \
         WHERE family_key = ?1",
        params![keep, drop],
    )?;
    // Rows that could not move duplicate a member of the surviving family.
    conn.execute(
        "DELETE FROM retention_staging_state WHERE family_key = ?1",
        [drop],
    )?;
    Ok(keep.to_owned())
}

fn candidate_hash(candidates: &BTreeMap<&str, &SidebarThread>) -> String {
    let mut hasher = Sha256::new();
    for (id, thread) in candidates {
        hasher.update(format!(
            "{id}|{}|{:?}\n",
            thread.archived, thread.archived_at_s
        ));
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn members(
    conn: &Connection,
    family_key: &str,
    kind: &str,
) -> Result<Vec<String>, StorageError> {
    let mut statement = conn.prepare_cached(
        "SELECT id FROM retention_staging WHERE family_key = ?1 AND kind = ?2 ORDER BY id",
    )?;
    let rows = statement.query_map(params![family_key, kind], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub(super) fn frontier_empty(conn: &Connection, family_key: &str) -> Result<bool, StorageError> {
    Ok(!conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM retention_staging \
         WHERE family_key = ?1 AND kind IN ('frontier_thread', 'frontier_run'))",
        [family_key],
        |row| row.get::<_, bool>(0),
    )?)
}

enum Expansion {
    Finished,
    OutOfBudget,
    Merged,
    Unresolved(String),
}

fn expand_family(
    conn: &Connection,
    family_key: &str,
    edges: &mut u64,
    max_edges: u64,
) -> Result<Expansion, StorageError> {
    let mut key = family_key.to_owned();
    loop {
        if *edges >= max_edges {
            return Ok(Expansion::OutOfBudget);
        }
        let item: Option<(String, String)> = conn
            .query_row(
                "SELECT kind, id FROM retention_staging \
                 WHERE family_key = ?1 AND kind IN ('frontier_thread', 'frontier_run') \
                 ORDER BY kind, id LIMIT 1",
                [&key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((kind, id)) = item else {
            return Ok(Expansion::Finished);
        };
        let before = *edges;
        let neighbors = if kind == "frontier_thread" {
            thread_neighbors(conn, &id, edges)?
        } else {
            run_neighbors(conn, &id, edges)?
        };
        conn.execute(
            "DELETE FROM retention_staging WHERE family_key = ?1 AND kind = ?2 AND id = ?3",
            params![key, kind, id],
        )?;
        conn.execute(
            "UPDATE retention_staging_state SET explored_edges = explored_edges + ?2 \
             WHERE family_key = ?1",
            params![key, i64::try_from(*edges - before).unwrap_or(i64::MAX)],
        )?;
        let mut merged = false;
        for neighbor in neighbors {
            let (member_kind, member) = match neighbor {
                Neighbor::Thread(id) => ("thread", id),
                Neighbor::Run(id) => ("run", id),
                Neighbor::Gone(id) => return Ok(Expansion::Unresolved(id)),
            };
            let (surviving, _) = add_member(conn, &key, member_kind, &member)?;
            if surviving != key {
                merged = true;
                key = surviving;
            }
        }
        if merged {
            return Ok(Expansion::Merged);
        }
    }
}

/// Advances the closure search of every archived, unpinned thread's family by
/// at most `budget.max_edges` rows and reports each family's state.
pub(crate) fn explore(
    conn: &Connection,
    input: &RetentionInput,
    policy: &ThreadRetentionPolicy,
    budget: ExploreBudget,
) -> Result<Vec<(String, FamilyState)>, StorageError> {
    if policy.archived_delete_after_days == 0 {
        conn.execute("DELETE FROM retention_staging_state", [])?;
        return Ok(Vec::new());
    }
    let candidates: BTreeMap<&str, &SidebarThread> = input
        .threads
        .iter()
        .filter(|thread| thread.archived && !thread.pinned)
        .map(|thread| (thread.thread_id.as_str(), thread))
        .collect();
    if backfill_pending(conn)? {
        return Ok(candidates
            .keys()
            .map(|id| {
                (
                    (*id).to_owned(),
                    FamilyState::Blocked(BlockReason::Exploring),
                )
            })
            .collect());
    }
    let hash = candidate_hash(&candidates);
    let mut results: BTreeMap<String, FamilyState> = BTreeMap::new();
    // Seed a family for every candidate that is not a member of one yet.
    for id in candidates.keys() {
        let known: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM retention_staging WHERE kind = 'thread' AND id = ?1) \
             OR EXISTS(SELECT 1 FROM thread_tombstones WHERE thread_id = ?1)",
            [id],
            |row| row.get(0),
        )?;
        if known {
            continue;
        }
        conn.execute(
            "INSERT INTO retention_staging_state(family_key, started_at_ns, input_hash) \
             VALUES (?1, ?2, ?3)",
            params![id, input.now_ns, hash],
        )?;
        add_member(conn, id, "thread", id)?;
    }
    conn.execute(
        "UPDATE retention_staging_state SET input_hash = ?1 WHERE input_hash <> ?1",
        [&hash],
    )?;

    let mut edges = 0_u64;
    loop {
        let keys = family_keys(conn)?;
        let mut restart = false;
        for key in keys {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM retention_staging_state WHERE family_key = ?1)",
                [&key],
                |row| row.get(0),
            )?;
            if !exists {
                continue;
            }
            match expand_family(conn, &key, &mut edges, budget.max_edges)? {
                Expansion::Finished => {}
                Expansion::OutOfBudget => break,
                Expansion::Merged => restart = true,
                Expansion::Unresolved(id) => {
                    conn.execute(
                        "DELETE FROM retention_staging_state WHERE family_key = ?1",
                        [&key],
                    )?;
                    results.insert(key, FamilyState::Blocked(BlockReason::FamilyUnresolved(id)));
                }
            }
        }
        if !restart || edges >= budget.max_edges {
            break;
        }
    }

    let no_owners = BTreeMap::new();
    for key in family_keys(conn)? {
        let state = if frontier_empty(conn, &key)? {
            let threads = members(conn, &key, "thread")?;
            let runs = members(conn, &key, "run")?;
            match family_blocked(conn, &key, &threads, input, policy, &no_owners)? {
                Some(reason) => FamilyState::Blocked(reason),
                None => FamilyState::Explored { threads, runs },
            }
        } else {
            let explored: i64 = conn.query_row(
                "SELECT explored_edges FROM retention_staging_state WHERE family_key = ?1",
                [&key],
                |row| row.get(0),
            )?;
            FamilyState::Exploring {
                explored_edges: u64::try_from(explored).unwrap_or(0),
            }
        };
        results.insert(key, state);
    }
    Ok(results.into_iter().collect())
}

fn family_keys(conn: &Connection) -> Result<Vec<String>, StorageError> {
    let mut statement =
        conn.prepare_cached("SELECT family_key FROM retention_staging_state ORDER BY family_key")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Helper for `finalize`: how the staged sets look to a fixed-point check.
pub(super) fn staged_sets(
    conn: &Connection,
    family_key: &str,
) -> Result<(BTreeSet<String>, BTreeSet<String>), StorageError> {
    Ok((
        members(conn, family_key, "thread")?.into_iter().collect(),
        members(conn, family_key, "run")?.into_iter().collect(),
    ))
}
