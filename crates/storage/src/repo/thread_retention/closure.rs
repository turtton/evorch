//! Family closure: the threads and runs that must be deleted together.
//!
//! A family is the connected set of threads and runs under every relation
//! that ties their rows together (lineage, bindings, the canonical run→thread
//! rule, delegation, usage parents, escalation). The search is a worklist kept
//! in `retention_staging` so it can stop and resume across startups, **inside** a
//! node: each node's relations are a fixed list of edge queries paged by rowid,
//! and the position reached (query, last rowid) is stored on the worklist item.
//! Every statement costs the rows it returns (at least one) and is limited to the
//! budget that is left, so a call never reads more than its budget (a run's
//! canonical-thread walk, at most `MAX_RUN_LINKS` + 1 links, is the one
//! indivisible step).

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use super::candidates::family_blocked;
use crate::StorageError;
use crate::migrations::backfill::backfill_pending;
use crate::repo::threads::{CanonicalThread, canonical_thread};
use crate::thread_retention::{
    BlockReason, ExploreBudget, FamilyState, RetentionInput, SidebarThread, ThreadRetentionPolicy,
};

/// Rows one edge query returns at most, whatever the budget.
const PAGE_ROWS: u64 = 512;

/// A member reached from another member, or a deleted/unresolvable identity in the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Neighbor {
    Thread(String),
    Run(String),
    /// Already tombstoned, or not resolvable (cycle, too deep): the family
    /// cannot be confirmed.
    Gone(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NodeKind {
    Thread,
    Run,
}

/// Where an interrupted expansion resumes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Position {
    pub step: u32,
    pub after: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Expansion {
    Done,
    /// The budget ran out; resume here.
    Budget(Position),
    /// The sink asked to stop (an unresolvable neighbor).
    Stopped,
}

#[derive(Clone, Copy)]
enum Yields {
    Thread,
    Run,
}

enum Step {
    /// The thread's own row: parent thread, root run and escalation source run.
    ThreadOwn,
    /// The canonical run→thread rule.
    Walk,
    /// `sql` takes (key, after, limit) and returns (rowid, value).
    Paged(&'static str, Yields),
}

const THREAD_STEPS: &[Step] = &[
    Step::ThreadOwn,
    Step::Paged(
        "SELECT rowid, thread_id FROM threads WHERE parent_thread_id = ?1 AND rowid > ?2 \
         ORDER BY rowid LIMIT ?3",
        Yields::Thread,
    ),
    Step::Paged(
        "SELECT rowid, run_id FROM thread_runs WHERE thread_id = ?1 AND rowid > ?2 \
         ORDER BY rowid LIMIT ?3",
        Yields::Run,
    ),
    // Conversation roots named for the thread, the same predicate as `runs_of_thread`.
    Step::Paged(
        "SELECT rowid, run_id FROM run_contexts \
         WHERE parent_run_id IS NULL AND substr(name, 1, 5) = 'chat:' \
         AND instr(substr(name, 6), ':') > 1 \
         AND substr(name, 6 + instr(substr(name, 6), ':')) = ?1 AND rowid > ?2 \
         ORDER BY rowid LIMIT ?3",
        Yields::Run,
    ),
];

const RUN_STEPS: &[Step] = &[
    Step::Walk,
    Step::Paged(
        "SELECT rowid, thread_id FROM thread_runs WHERE run_id = ?1 AND rowid > ?2 \
         ORDER BY rowid LIMIT ?3",
        Yields::Thread,
    ),
    Step::Paged(
        "SELECT rowid, thread_id FROM threads WHERE escalation_source_run_id = ?1 AND rowid > ?2 \
         ORDER BY rowid LIMIT ?3",
        Yields::Thread,
    ),
    Step::Paged(
        "SELECT rowid, thread_id FROM threads WHERE root_run_id = ?1 AND rowid > ?2 \
         ORDER BY rowid LIMIT ?3",
        Yields::Thread,
    ),
    Step::Paged(
        "SELECT rowid, parent_run_id FROM run_contexts \
         WHERE run_id = ?1 AND parent_run_id IS NOT NULL AND rowid > ?2 ORDER BY rowid LIMIT ?3",
        Yields::Run,
    ),
    Step::Paged(
        "SELECT rowid, run_id FROM run_contexts WHERE parent_run_id = ?1 AND rowid > ?2 \
         ORDER BY rowid LIMIT ?3",
        Yields::Run,
    ),
    Step::Paged(
        "SELECT rowid, run_id FROM usage_requests \
         WHERE parent_run_id = ?1 AND run_id IS NOT NULL AND rowid > ?2 ORDER BY rowid LIMIT ?3",
        Yields::Run,
    ),
    Step::Paged(
        "SELECT rowid, parent_run_id FROM usage_requests \
         WHERE run_id = ?1 AND parent_run_id IS NOT NULL AND rowid > ?2 ORDER BY rowid LIMIT ?3",
        Yields::Run,
    ),
];

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

type Sink<'a> = dyn FnMut(Neighbor) -> Result<bool, StorageError> + 'a;

/// Walks the relations of one node from `start`, feeding every neighbor to
/// `sink`, until the node is done, the budget (`max_edges` in total, `edges` already
/// used) is spent, or the sink stops it.
pub(super) fn expand_node(
    conn: &Connection,
    kind: NodeKind,
    id: &str,
    start: Position,
    edges: &mut u64,
    max_edges: u64,
    sink: &mut Sink<'_>,
) -> Result<Expansion, StorageError> {
    let steps = match kind {
        NodeKind::Thread => THREAD_STEPS,
        NodeKind::Run => RUN_STEPS,
    };
    let mut after = start.after;
    for (index, step) in steps.iter().enumerate().skip(start.step as usize) {
        let here = |after| Position {
            step: u32::try_from(index).unwrap_or(u32::MAX),
            after,
        };
        match step {
            Step::ThreadOwn => {
                if *edges >= max_edges {
                    return Ok(Expansion::Budget(here(0)));
                }
                *edges += 1;
                let own = conn
                    .query_row(
                        "SELECT parent_thread_id, root_run_id, escalation_source_run_id \
                         FROM threads WHERE thread_id = ?1",
                        [id],
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
                    if let Some(parent) = parent
                        && !sink(thread_neighbor(conn, parent)?)?
                    {
                        return Ok(Expansion::Stopped);
                    }
                    for run in [root, source].into_iter().flatten() {
                        if !sink(run_neighbor(conn, run)?)? {
                            return Ok(Expansion::Stopped);
                        }
                    }
                }
            }
            Step::Walk => {
                if *edges >= max_edges {
                    return Ok(Expansion::Budget(here(0)));
                }
                let mut links = 0;
                let resolved = canonical_thread(conn, id, &mut links)?;
                *edges += links.max(1);
                let neighbor = match resolved {
                    CanonicalThread::Thread(thread) => Some(thread_neighbor(conn, thread)?),
                    CanonicalThread::None => None,
                    CanonicalThread::Broken => Some(Neighbor::Gone(id.to_owned())),
                };
                if let Some(neighbor) = neighbor
                    && !sink(neighbor)?
                {
                    return Ok(Expansion::Stopped);
                }
            }
            Step::Paged(sql, yields) => loop {
                if *edges >= max_edges {
                    return Ok(Expansion::Budget(here(after)));
                }
                let limit = (max_edges - *edges).min(PAGE_ROWS);
                let mut statement = conn.prepare_cached(sql)?;
                let rows: Vec<(i64, Option<String>)> = statement
                    .query_map(
                        params![id, after, i64::try_from(limit).unwrap_or(i64::MAX)],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )?
                    .collect::<Result<_, _>>()?;
                drop(statement);
                let count = rows.len() as u64;
                *edges += count.max(1);
                for (rowid, value) in rows {
                    after = rowid;
                    let Some(value) = value else { continue };
                    let neighbor = match yields {
                        Yields::Thread => thread_neighbor(conn, value)?,
                        Yields::Run => run_neighbor(conn, value)?,
                    };
                    if !sink(neighbor)? {
                        return Ok(Expansion::Stopped);
                    }
                }
                if count < limit {
                    break;
                }
            },
        }
        after = 0;
    }
    Ok(Expansion::Done)
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

pub(super) enum FamilyOutcome {
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
) -> Result<FamilyOutcome, StorageError> {
    let mut key = family_key.to_owned();
    loop {
        if *edges >= max_edges {
            return Ok(FamilyOutcome::OutOfBudget);
        }
        let item: Option<(String, String, i64, i64)> = conn
            .query_row(
                "SELECT kind, id, edge_step, edge_after FROM retention_staging \
                 WHERE family_key = ?1 AND kind IN ('frontier_thread', 'frontier_run') \
                 ORDER BY kind, id LIMIT 1",
                [&key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((kind, id, step, after)) = item else {
            return Ok(FamilyOutcome::Finished);
        };
        let node = if kind == "frontier_thread" {
            NodeKind::Thread
        } else {
            NodeKind::Run
        };
        let before = *edges;
        let mut merged = false;
        let mut unresolved = None;
        let outcome = expand_node(
            conn,
            node,
            &id,
            Position {
                step: u32::try_from(step).unwrap_or(0),
                after,
            },
            edges,
            max_edges,
            &mut |neighbor| {
                let (member_kind, member) = match neighbor {
                    Neighbor::Thread(id) => ("thread", id),
                    Neighbor::Run(id) => ("run", id),
                    Neighbor::Gone(id) => {
                        unresolved = Some(id);
                        return Ok(false);
                    }
                };
                let (surviving, _) = add_member(conn, &key, member_kind, &member)?;
                if surviving != key {
                    merged = true;
                    key = surviving;
                }
                Ok(true)
            },
        )?;
        if let Some(id) = unresolved {
            return Ok(FamilyOutcome::Unresolved(id));
        }
        conn.execute(
            "UPDATE retention_staging_state SET explored_edges = explored_edges + ?2 \
             WHERE family_key = ?1",
            params![key, i64::try_from(*edges - before).unwrap_or(i64::MAX)],
        )?;
        match outcome {
            Expansion::Done => {
                conn.execute(
                    "DELETE FROM retention_staging WHERE family_key = ?1 AND kind = ?2 AND id = ?3",
                    params![key, kind, id],
                )?;
            }
            Expansion::Budget(position) => {
                conn.execute(
                    "UPDATE retention_staging SET edge_step = ?4, edge_after = ?5 \
                     WHERE family_key = ?1 AND kind = ?2 AND id = ?3",
                    params![key, kind, id, position.step, position.after],
                )?;
                return Ok(if merged {
                    FamilyOutcome::Merged
                } else {
                    FamilyOutcome::OutOfBudget
                });
            }
            Expansion::Stopped => {}
        }
        if merged {
            return Ok(FamilyOutcome::Merged);
        }
    }
}

/// Advances the closure search of every archived, unpinned thread's family by
/// at most `budget.max_edges` rows and reports each family's state. The whole call
/// is one transaction: a failure leaves the previous resumable state untouched.
pub(crate) fn explore(
    conn: &Connection,
    input: &RetentionInput,
    policy: &ThreadRetentionPolicy,
    budget: ExploreBudget,
) -> Result<Vec<(String, FamilyState)>, StorageError> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let states = explore_in(&tx, input, policy, budget)?;
    tx.commit()?;
    Ok(states)
}

fn explore_in(
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
                FamilyOutcome::Finished => {}
                FamilyOutcome::OutOfBudget => break,
                FamilyOutcome::Merged => restart = true,
                FamilyOutcome::Unresolved(id) => {
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
