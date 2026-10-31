//! Confirming a family's deletion: the fixed-point check and the one commit
//! that makes the deletion irreversible.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::candidates::family_blocked;
use super::closure::{
    Expansion, Neighbor, NodeKind, Position, add_member, expand_node, frontier_empty, staged_sets,
};
use crate::StorageError;
use crate::migrations::backfill::backfill_pending;
use crate::thread_retention::{
    BindingInput, BlockReason, ConfirmedFamily, FinalizeBudget, FinalizeOutcome, RetentionInput,
    ThreadRetentionPolicy,
};

/// Re-checks that the staged family is closed, re-verifies every protection
/// and, if all hold, writes the tombstones and removes the family from the
/// live tables in one transaction. Nothing is confirmed unless the whole
/// transaction commits.
pub(crate) fn finalize(
    conn: &Connection,
    family_key: &str,
    input: &RetentionInput,
    policy: &ThreadRetentionPolicy,
    owners: &BTreeMap<String, String>,
    binding: &BindingInput,
    budget: FinalizeBudget,
) -> Result<FinalizeOutcome, StorageError> {
    if policy.archived_delete_after_days == 0 {
        return Ok(FinalizeOutcome::Disabled);
    }
    if backfill_pending(conn)? {
        return Ok(FinalizeOutcome::Blocked(BlockReason::Exploring));
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let staged: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM retention_staging_state WHERE family_key = ?1)",
        [family_key],
        |row| row.get(0),
    )?;
    if !staged || !frontier_empty(&tx, family_key)? {
        return Ok(FinalizeOutcome::Blocked(BlockReason::Exploring));
    }
    let (threads, runs) = staged_sets(&tx, family_key)?;
    let members = (threads.len() + runs.len()) as u64;

    // Fixed point: no relation of any member leads outside the staged set. The
    // check reads the same edges the exploration did, within the caller's budget,
    // and stops as soon as the budget is spent.
    let mut edges = 0_u64;
    let mut outside: BTreeSet<(&'static str, String)> = BTreeSet::new();
    let mut unresolved: Option<String> = None;
    let nodes = threads
        .iter()
        .map(|id| (NodeKind::Thread, id))
        .chain(runs.iter().map(|id| (NodeKind::Run, id)));
    for (kind, id) in nodes {
        let expansion = expand_node(
            &tx,
            kind,
            id,
            Position::default(),
            &mut edges,
            budget.max_edges,
            &mut |neighbor| {
                match neighbor {
                    Neighbor::Gone(gone) => {
                        unresolved = Some(gone);
                        return Ok(false);
                    }
                    Neighbor::Thread(other) if !threads.contains(&other) => {
                        outside.insert(("thread", other));
                    }
                    Neighbor::Run(other) if !runs.contains(&other) => {
                        outside.insert(("run", other));
                    }
                    Neighbor::Thread(_) | Neighbor::Run(_) => {}
                }
                Ok(true)
            },
        )?;
        if let Some(gone) = unresolved {
            return Ok(FinalizeOutcome::Blocked(BlockReason::FamilyUnresolved(
                gone,
            )));
        }
        if let Expansion::Budget(_) = expansion {
            return Ok(FinalizeOutcome::OverBudget {
                members,
                edges_read: edges,
            });
        }
    }
    if !outside.is_empty() {
        // The family grew after it was explored. Stage what is new so the next
        // exploration continues from it; nothing is confirmed.
        let mut key = family_key.to_owned();
        for (kind, id) in &outside {
            key = add_member(&tx, &key, kind, id)?.0;
        }
        tx.commit()?;
        return Ok(FinalizeOutcome::Stale {
            new_members: outside.len() as u64,
        });
    }

    let thread_list: Vec<String> = threads.into_iter().collect();
    let run_list: Vec<String> = runs.into_iter().collect();
    if let Some(reason) = family_blocked(&tx, family_key, &thread_list, input, policy, owners)? {
        return Ok(FinalizeOutcome::Blocked(reason));
    }

    let task_list = family_tasks(&tx, &run_list)?;
    let estimate_bytes = estimate(&tx, &run_list)?;
    for thread in &thread_list {
        tx.execute(
            "INSERT INTO thread_tombstones(thread_id, family_key, deleted_at_ns) VALUES (?1, ?2, ?3)",
            params![thread, family_key, input.now_ns],
        )?;
    }
    for run in &run_list {
        let thread: Option<String> = tx
            .query_row(
                "SELECT thread_id FROM thread_runs WHERE run_id = ?1",
                [run],
                |row| row.get(0),
            )
            .optional()?;
        tx.execute(
            "INSERT INTO run_tombstones(run_id, thread_id, family_key) VALUES (?1, ?2, ?3)",
            params![run, thread.as_deref().unwrap_or(family_key), family_key],
        )?;
    }
    for task in &task_list {
        tx.execute(
            "INSERT INTO task_tombstones(task_id, family_key) VALUES (?1, ?2)",
            params![task, family_key],
        )?;
    }
    tx.execute(
        "INSERT INTO reclaim_jobs(family_key, state, cursor_json, created_at_ns, updated_at_ns, \
         estimate_bytes) VALUES (?1, 'db_pending', '{}', ?2, ?2, ?3)",
        params![
            family_key,
            input.now_ns,
            i64::try_from(estimate_bytes).unwrap_or(i64::MAX)
        ],
    )?;
    for thread in &thread_list {
        tx.execute("DELETE FROM thread_runs WHERE thread_id = ?1", [thread])?;
        tx.execute("DELETE FROM threads WHERE thread_id = ?1", [thread])?;
    }
    for run in &run_list {
        tx.execute("DELETE FROM thread_runs WHERE run_id = ?1", [run])?;
    }
    tx.execute(
        "DELETE FROM retention_staging_state WHERE family_key = ?1",
        [family_key],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO retention_binding(id, ownership_root, sidebar_path, bound_at_ns) \
         VALUES (1, ?1, ?2, ?3)",
        params![binding.ownership_root, binding.sidebar_path, input.now_ns],
    )?;
    tx.commit()?;
    Ok(FinalizeOutcome::Confirmed(ConfirmedFamily {
        family_key: family_key.to_owned(),
        threads: thread_list,
        runs: run_list,
        tasks: task_list,
        estimate_bytes,
    }))
}

/// Tasks owned by the family's runs.
fn family_tasks(conn: &Connection, runs: &[String]) -> Result<Vec<String>, StorageError> {
    let mut tasks = BTreeSet::new();
    let mut statement = conn.prepare("SELECT id FROM tasks WHERE parent_run_id = ?1")?;
    for run in runs {
        for id in statement.query_map([run], |row| row.get::<_, String>(0))? {
            tasks.insert(id?);
        }
    }
    Ok(tasks.into_iter().collect())
}

/// Stored bytes of the family's events and run contexts: a measurement for
/// display, not a bound.
fn estimate(conn: &Connection, runs: &[String]) -> Result<u64, StorageError> {
    let mut total = 0_u64;
    let mut events = conn
        .prepare("SELECT COALESCE(SUM(OCTET_LENGTH(payload)), 0) FROM events WHERE run_id = ?1")?;
    let mut contexts = conn.prepare(
        "SELECT COALESCE(SUM(OCTET_LENGTH(messages_json) + OCTET_LENGTH(checkpoints_json)), 0) \
         FROM run_contexts WHERE run_id = ?1",
    )?;
    for run in runs {
        total = total
            .saturating_add(events.query_row([run], |row| row.get::<_, u64>(0))?)
            .saturating_add(contexts.query_row([run], |row| row.get::<_, u64>(0))?);
    }
    Ok(total)
}
