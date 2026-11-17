//! The plan tables: one transaction per revision, and the reads.
//!
//! A write runs, in one IMMEDIATE transaction: thread tombstone check, binding and
//! revision compare-and-swap, validation of the whole batch, the projection rewrite, the
//! change event and the history index row. A failure of any step, the event's size or
//! secret check included, rolls everything back. The change event is stored here and
//! nowhere else: the recorder skips `EventKind::ConversationPlan`, so the bus copy that the
//! caller publishes after commit is only a notification.

use std::collections::HashMap;

use event_bus::{
    ConversationPlanEvent, Event, EventKind, PlanChangeCause, PlanProgress, PlanStatusValues,
    PlanTaskStatus, PlanWeight,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::conversation_plan::{
    Node, PLAN_HISTORY_DEFAULT_ENTRIES, PLAN_HISTORY_MAX_ENTRIES, PLAN_PAGE_DEFAULT_NODES,
    PLAN_PAGE_MAX_BYTES, PLAN_PAGE_MAX_NODES, PlanApplied, PlanHistoryEntry, PlanHistoryPage,
    PlanNodeDetail, PlanNodeView, PlanOperation, PlanRejection, PlanSummary, PlanTaskRead,
    PlanTree, PlanTreePage, PlanUpdate, operation_texts, plan_id_for_thread,
};
use crate::entity::SecretGuard;
use crate::repo::event::{EventAccounting, append_event_in_transaction, row_to_event};
use crate::{HardLimits, StorageError};

/// What a write asks for.
pub(crate) enum WriteRequest {
    /// The model's batch.
    Update(PlanUpdate),
    /// The runtime's reconciliation: every `in_progress` leaf becomes `interrupted`.
    Reconcile {
        thread_id: String,
        source_run_id: Option<String>,
    },
}

/// The result of a write and what it added to the event log (rows, payload bytes).
pub(crate) type WriteOutcome = (PlanApplied, u64, u64);

struct PlanRow {
    plan_id: String,
    revision: u64,
    next_seq: u64,
    updated_ns: i64,
}

fn reject(rejection: PlanRejection) -> StorageError {
    StorageError::ConversationPlan(rejection)
}

fn to_u64(value: i64, what: &'static str) -> Result<u64, StorageError> {
    u64::try_from(value).map_err(|_| StorageError::OutOfRange(what))
}

fn to_i64(value: u64, what: &'static str) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::OutOfRange(what))
}

fn ensure_live_thread(conn: &Connection, thread_id: &str) -> Result<(), StorageError> {
    let deleted: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM thread_tombstones WHERE thread_id = ?1)",
        [thread_id],
        |row| row.get(0),
    )?;
    if deleted {
        return Err(StorageError::Tombstoned {
            run_id: None,
            thread_id: Some(thread_id.to_owned()),
        });
    }
    Ok(())
}

fn plan_row(conn: &Connection, thread_id: &str) -> Result<Option<PlanRow>, StorageError> {
    let row = conn
        .query_row(
            "SELECT plan_id, revision, next_task_seq, updated_ns FROM conversation_plans WHERE thread_id = ?1",
            [thread_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    row.map(|(plan_id, revision, next_seq, updated_ns)| {
        Ok(PlanRow {
            plan_id,
            revision: to_u64(revision, "plan revision")?,
            next_seq: to_u64(next_seq, "plan task sequence")?,
            updated_ns,
        })
    })
    .transpose()
}

fn status_of(text: Option<String>) -> Result<Option<PlanTaskStatus>, StorageError> {
    text.map(|text| {
        PlanTaskStatus::parse(&text)
            .ok_or_else(|| StorageError::Serialization(format!("unknown plan status {text}")))
    })
    .transpose()
}

fn weight_of(value: i64) -> Result<PlanWeight, StorageError> {
    PlanWeight::new(to_u64(value, "plan weight")?)
        .map_err(|error| StorageError::Serialization(error.to_string()))
}

fn load_tree(conn: &Connection, row: &PlanRow) -> Result<PlanTree, StorageError> {
    let mut tree = PlanTree {
        plan_id: row.plan_id.clone(),
        nodes: HashMap::new(),
        roots: Vec::new(),
        next_seq: row.next_seq,
    };
    let mut statement = conn.prepare_cached(
        "SELECT task_id, parent_id, title, description, result, blocker, weight, status \
         FROM conversation_plan_nodes WHERE plan_id = ?1 ORDER BY ord",
    )?;
    let mut rows = statement.query([&row.plan_id])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let parent: Option<String> = row.get(1)?;
        let node = Node {
            id: id.clone(),
            parent: parent.clone(),
            children: Vec::new(),
            title: row.get(2)?,
            description: row.get(3)?,
            result: row.get(4)?,
            blocker: row.get(5)?,
            weight: weight_of(row.get(6)?)?,
            status: status_of(row.get(7)?)?,
        };
        match &parent {
            None => tree.roots.push(id.clone()),
            Some(parent) => tree
                .nodes
                .get_mut(parent)
                .ok_or_else(|| {
                    StorageError::Serialization("plan task stored before its parent".into())
                })?
                .children
                .push(id.clone()),
        }
        tree.nodes.insert(id, node);
    }
    Ok(tree)
}

fn guard_texts(operations: &[PlanOperation]) -> Result<(), StorageError> {
    let guard = SecretGuard::from_env();
    for (field, text) in operation_texts(operations) {
        guard.check_text("conversation_plan", field, text)?;
    }
    Ok(())
}

fn write_projection(
    tx: &Transaction<'_>,
    row: Option<&PlanRow>,
    thread_id: &str,
    new: &PlanTree,
    done: &crate::conversation_plan::Finalized,
    revision: u64,
    wall_clock_ns: i64,
) -> Result<(), StorageError> {
    let progress_json = serde_json::to_string(&done.progress)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    let node_count = to_i64(done.progress.node_count.into(), "plan node count")?;
    let plan_bytes = to_i64(done.plan_bytes as u64, "plan bytes")?;
    let sequence = to_i64(new.next_seq, "plan task sequence")?;
    if row.is_some() {
        tx.execute(
            "UPDATE conversation_plans SET revision = ?2, next_task_seq = ?3, node_count = ?4, \
             plan_bytes = ?5, progress_json = ?6, updated_ns = ?7 WHERE plan_id = ?1",
            params![
                new.plan_id,
                to_i64(revision, "plan revision")?,
                sequence,
                node_count,
                plan_bytes,
                progress_json,
                wall_clock_ns
            ],
        )?;
    } else {
        tx.execute(
            "INSERT INTO conversation_plans(plan_id, thread_id, revision, next_task_seq, \
             node_count, plan_bytes, progress_json, updated_ns) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                new.plan_id,
                thread_id,
                to_i64(revision, "plan revision")?,
                sequence,
                node_count,
                plan_bytes,
                progress_json,
                wall_clock_ns
            ],
        )?;
    }
    tx.execute(
        "DELETE FROM conversation_plan_nodes WHERE plan_id = ?1",
        [&new.plan_id],
    )?;
    let mut insert = tx.prepare_cached(
        "INSERT INTO conversation_plan_nodes(plan_id, task_id, ord, parent_id, position, depth, \
         title, description, result, blocker, weight, status, child_count, share_json) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
    )?;
    for (ord, entry) in done.flat.iter().enumerate() {
        let node = &new.nodes[&entry.id];
        let share = serde_json::to_string(&entry.share)
            .map_err(|error| StorageError::Serialization(error.to_string()))?;
        insert.execute(params![
            new.plan_id,
            node.id,
            to_i64(ord as u64, "plan task order")?,
            node.parent,
            entry.position,
            entry.depth,
            node.title,
            node.description,
            node.result,
            node.blocker,
            node.weight.get(),
            node.status.map(PlanTaskStatus::as_str),
            to_i64(node.children.len() as u64, "plan child count")?,
            share
        ])?;
    }
    Ok(())
}

/// Applies a write. The caller commits `accounting` only through this function's return:
/// it is replaced after the commit, never before.
pub(crate) fn write(
    conn: &Connection,
    request: WriteRequest,
    limits: &HardLimits,
    accounting: &mut EventAccounting,
) -> Result<WriteOutcome, StorageError> {
    let (thread_id, expected, source_run_id, operations, cause) = match request {
        WriteRequest::Update(update) => (
            update.thread_id,
            Some(update.expected_revision),
            update.source_run_id,
            Some(update.operations),
            PlanChangeCause::ModelUpdate,
        ),
        WriteRequest::Reconcile {
            thread_id,
            source_run_id,
        } => (
            thread_id,
            None,
            source_run_id,
            None,
            PlanChangeCause::RuntimeInterrupt,
        ),
    };
    if thread_id.is_empty() {
        return Err(StorageError::Serialization(
            "conversation plan needs a thread".into(),
        ));
    }
    if let Some(operations) = &operations {
        guard_texts(operations)?;
    }
    if let Some(run) = &source_run_id {
        SecretGuard::from_env().check_text("conversation_plan", "source_run_id", run)?;
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    ensure_live_thread(&tx, &thread_id)?;
    let existing = plan_row(&tx, &thread_id)?;
    let current = existing.as_ref().map_or(0, |row| row.revision);
    if let Some(expected) = expected
        && expected != current
    {
        return Err(StorageError::StaleRevision {
            kind: crate::conversation_plan::PLAN_REVISION_KIND,
            expected,
            current,
        });
    }
    let old = match &existing {
        Some(row) => load_tree(&tx, row)?,
        None => PlanTree::empty(plan_id_for_thread(&thread_id)),
    };
    if existing.is_none() {
        let taken: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM conversation_plans WHERE plan_id = ?1)",
            [&old.plan_id],
            |row| row.get(0),
        )?;
        if taken {
            return Err(reject(PlanRejection::BindingConflict));
        }
    }
    // A model batch is applied as sent (an empty one is rejected there). Only the runtime's
    // reconcile may find nothing to do.
    let operations = match operations {
        Some(operations) => operations,
        None => {
            let operations = old.interrupt_operations();
            if operations.is_empty() {
                let (_, progress) = old.flatten();
                return Ok((unchanged(existing.as_ref(), current, progress), 0, 0));
            }
            operations
        }
    };
    let (new, applied) = old
        .apply(&operations, cause == PlanChangeCause::RuntimeInterrupt)
        .map_err(reject)?;
    let done = new.finalize().map_err(reject)?;
    if new.same_tasks(&old) {
        return Ok((unchanged(existing.as_ref(), current, done.progress), 0, 0));
    }
    let revision = current + 1;
    let event = Event::new(EventKind::ConversationPlan(
        ConversationPlanEvent::Changed {
            thread_id: thread_id.clone(),
            plan_id: new.plan_id.clone(),
            revision,
            cause,
            source_run_id,
            changes: applied.changes,
            progress: done.progress,
        },
    ));
    let wall_clock_ns = crate::db::system_time_to_ns(event.meta.wall_clock)?;
    write_projection(
        &tx,
        existing.as_ref(),
        &thread_id,
        &new,
        &done,
        revision,
        wall_clock_ns,
    )?;
    let mut next_accounting = accounting.clone();
    let stored = append_event_in_transaction(&tx, None, &event, limits, &mut next_accounting)?;
    tx.execute(
        "INSERT INTO conversation_plan_history(plan_id, revision, event_id) VALUES (?1,?2,?3)",
        params![new.plan_id, to_i64(revision, "plan revision")?, stored.id],
    )?;
    plan_gate(PlanWritePoint::BeforeCommit)?;
    tx.commit()?;
    let bytes = next_accounting.last_event_bytes;
    *accounting = next_accounting;
    plan_gate(PlanWritePoint::AfterCommit)?;
    Ok((
        PlanApplied {
            plan_id: Some(new.plan_id.clone()),
            revision,
            changed: true,
            progress: done.progress,
            created: applied.created,
            event: Some(stored),
        },
        1,
        bytes,
    ))
}

#[cfg(feature = "plan-write-gate")]
use crate::plan_write_gate::PlanWritePoint;

/// A point of the write a test can hold; nothing in a build without the feature.
#[cfg(feature = "plan-write-gate")]
fn plan_gate(point: PlanWritePoint) -> Result<(), StorageError> {
    crate::plan_write_gate::reached(point)
}

#[cfg(not(feature = "plan-write-gate"))]
#[derive(Clone, Copy)]
enum PlanWritePoint {
    BeforeCommit,
    AfterCommit,
}

#[cfg(not(feature = "plan-write-gate"))]
#[allow(clippy::unnecessary_wraps)]
fn plan_gate(_point: PlanWritePoint) -> Result<(), StorageError> {
    Ok(())
}

fn unchanged(row: Option<&PlanRow>, revision: u64, progress: PlanProgress) -> PlanApplied {
    PlanApplied {
        plan_id: row.map(|row| row.plan_id.clone()),
        revision,
        changed: false,
        progress,
        created: Vec::new(),
        event: None,
    }
}

/// Moves a plan to another thread (a planned handoff to an escalation thread). The plan
/// keeps its identity, revision and history; no event is added. Returns whether a plan
/// moved. A target that already has a plan is a conflict and nothing moves.
pub(crate) fn rebind_thread(
    conn: &Connection,
    from_thread: &str,
    to_thread: &str,
) -> Result<bool, StorageError> {
    if from_thread == to_thread {
        return Ok(false);
    }
    // Both ends are checked in the caller's transaction, so a thread deleted before the
    // commit can neither give up nor receive a plan.
    ensure_live_thread(conn, from_thread)?;
    ensure_live_thread(conn, to_thread)?;
    if plan_row(conn, to_thread)?.is_some() {
        return Err(reject(PlanRejection::BindingConflict));
    }
    Ok(conn.execute(
        "UPDATE conversation_plans SET thread_id = ?2 WHERE thread_id = ?1",
        params![from_thread, to_thread],
    )? > 0)
}

fn read_tx(conn: &Connection) -> Result<Transaction<'_>, StorageError> {
    Ok(Transaction::new_unchecked(
        conn,
        TransactionBehavior::Deferred,
    )?)
}

fn progress_of(conn: &Connection, plan_id: &str) -> Result<PlanProgress, StorageError> {
    let json: String = conn.query_row(
        "SELECT progress_json FROM conversation_plans WHERE plan_id = ?1",
        [plan_id],
        |row| row.get(0),
    )?;
    serde_json::from_str(&json).map_err(|error| StorageError::Serialization(error.to_string()))
}

pub(crate) fn summary(conn: &Connection, thread_id: &str) -> Result<PlanSummary, StorageError> {
    let tx = read_tx(conn)?;
    ensure_live_thread(&tx, thread_id)?;
    Ok(match plan_row(&tx, thread_id)? {
        None => PlanSummary {
            thread_id: thread_id.to_owned(),
            plan_id: None,
            revision: 0,
            progress: PlanProgress::default(),
            updated_at_unix_ms: None,
        },
        Some(row) => PlanSummary {
            thread_id: thread_id.to_owned(),
            progress: progress_of(&tx, &row.plan_id)?,
            plan_id: Some(row.plan_id),
            revision: row.revision,
            updated_at_unix_ms: Some(row.updated_ns.div_euclid(1_000_000)),
        },
    })
}

fn share_of(text: &str) -> Result<PlanStatusValues, StorageError> {
    serde_json::from_str(text).map_err(|error| StorageError::Serialization(error.to_string()))
}

fn cursor_text(plan_id: &str, revision: u64, ord: i64) -> String {
    format!("{plan_id}@{revision}@{ord}")
}

fn parse_cursor(text: &str, plan_id: &str) -> Result<(u64, i64), StorageError> {
    let mut parts = text.rsplitn(3, '@');
    let (Some(ord), Some(revision), Some(owner)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(reject(PlanRejection::InvalidCursor));
    };
    let (Ok(ord), Ok(revision)) = (ord.parse::<i64>(), revision.parse::<u64>()) else {
        return Err(reject(PlanRejection::InvalidCursor));
    };
    if owner != plan_id || ord < 0 {
        return Err(reject(PlanRejection::InvalidCursor));
    }
    Ok((revision, ord))
}

pub(crate) fn tree_page(
    conn: &Connection,
    thread_id: &str,
    cursor: Option<&str>,
    limit: Option<u32>,
) -> Result<PlanTreePage, StorageError> {
    let tx = read_tx(conn)?;
    ensure_live_thread(&tx, thread_id)?;
    let Some(row) = plan_row(&tx, thread_id)? else {
        if cursor.is_some() {
            return Err(reject(PlanRejection::InvalidCursor));
        }
        return Ok(PlanTreePage {
            plan_id: None,
            revision: 0,
            progress: PlanProgress::default(),
            nodes: Vec::new(),
            next_cursor: None,
        });
    };
    let start = match cursor {
        None => 0,
        Some(text) => {
            let (revision, ord) = parse_cursor(text, &row.plan_id)?;
            if revision != row.revision {
                return Err(reject(PlanRejection::StaleCursor {
                    cursor_revision: revision,
                    current: row.revision,
                }));
            }
            ord
        }
    };
    let limit = limit
        .unwrap_or(PLAN_PAGE_DEFAULT_NODES)
        .clamp(1, PLAN_PAGE_MAX_NODES);
    let mut statement = tx.prepare_cached(
        "SELECT ord, task_id, parent_id, position, depth, title, weight, status, child_count, \
         share_json FROM conversation_plan_nodes WHERE plan_id = ?1 AND ord >= ?2 \
         ORDER BY ord LIMIT ?3",
    )?;
    let mut rows = statement.query(params![row.plan_id, start, i64::from(limit) + 1])?;
    let mut nodes = Vec::new();
    let mut bytes = 0_usize;
    let mut next_cursor = None;
    while let Some(entry) = rows.next()? {
        let ord: i64 = entry.get(0)?;
        let view = PlanNodeView {
            task_id: entry.get(1)?,
            parent_id: entry.get(2)?,
            position: entry.get(3)?,
            depth: entry.get(4)?,
            title: entry.get(5)?,
            weight: weight_of(entry.get(6)?)?,
            status: status_of(entry.get(7)?)?,
            child_count: entry.get(8)?,
            progress_ppm: share_of(&entry.get::<_, String>(9)?)?,
        };
        let size = serde_json::to_string(&view)
            .map_err(|error| StorageError::Serialization(error.to_string()))?
            .len();
        if nodes.len() >= limit as usize
            || (!nodes.is_empty() && bytes + size > PLAN_PAGE_MAX_BYTES)
        {
            next_cursor = Some(cursor_text(&row.plan_id, row.revision, ord));
            break;
        }
        bytes += size;
        nodes.push(view);
    }
    drop(rows);
    drop(statement);
    Ok(PlanTreePage {
        progress: progress_of(&tx, &row.plan_id)?,
        plan_id: Some(row.plan_id),
        revision: row.revision,
        nodes,
        next_cursor,
    })
}

pub(crate) fn task(
    conn: &Connection,
    thread_id: &str,
    task_id: &str,
) -> Result<PlanTaskRead, StorageError> {
    let tx = read_tx(conn)?;
    ensure_live_thread(&tx, thread_id)?;
    let row = plan_row(&tx, thread_id)?
        .ok_or_else(|| reject(PlanRejection::UnknownTask(task_id.to_owned())))?;
    if task_id.contains(':') && !task_id.starts_with(&format!("{}:", row.plan_id)) {
        return Err(reject(PlanRejection::ForeignTask(task_id.to_owned())));
    }
    let node = tx
        .query_row(
            "SELECT parent_id, position, depth, title, description, result, blocker, weight, \
             status, share_json FROM conversation_plan_nodes WHERE plan_id = ?1 AND task_id = ?2",
            params![row.plan_id, task_id],
            |entry| {
                Ok((
                    entry.get::<_, Option<String>>(0)?,
                    entry.get::<_, u32>(1)?,
                    entry.get::<_, u32>(2)?,
                    entry.get::<_, String>(3)?,
                    entry.get::<_, String>(4)?,
                    entry.get::<_, String>(5)?,
                    entry.get::<_, String>(6)?,
                    entry.get::<_, i64>(7)?,
                    entry.get::<_, Option<String>>(8)?,
                    entry.get::<_, String>(9)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| reject(PlanRejection::UnknownTask(task_id.to_owned())))?;
    let child_ids = {
        let mut statement = tx.prepare_cached(
            "SELECT task_id FROM conversation_plan_nodes WHERE plan_id = ?1 AND parent_id = ?2 \
             ORDER BY position",
        )?;
        statement
            .query_map(params![row.plan_id, task_id], |entry| entry.get(0))?
            .collect::<Result<Vec<String>, _>>()?
    };
    Ok(PlanTaskRead {
        node: PlanNodeDetail {
            task_id: task_id.to_owned(),
            parent_id: node.0,
            position: node.1,
            depth: node.2,
            title: node.3,
            description: node.4,
            result: node.5,
            blocker: node.6,
            weight: weight_of(node.7)?,
            status: status_of(node.8)?,
            child_ids,
            progress_ppm: share_of(&node.9)?,
        },
        plan_id: row.plan_id,
        revision: row.revision,
    })
}

/// Revisions after `after_revision`, oldest first, found through the history index. An
/// event that retention reclaimed (its thread's deletion) stays an entry without a body.
pub(crate) fn history(
    conn: &Connection,
    thread_id: &str,
    after_revision: Option<u64>,
    limit: Option<u32>,
) -> Result<PlanHistoryPage, StorageError> {
    let tx = read_tx(conn)?;
    ensure_live_thread(&tx, thread_id)?;
    let Some(row) = plan_row(&tx, thread_id)? else {
        return Ok(PlanHistoryPage {
            plan_id: None,
            revision: 0,
            entries: Vec::new(),
            next_after_revision: None,
        });
    };
    let limit = limit
        .unwrap_or(PLAN_HISTORY_DEFAULT_ENTRIES)
        .clamp(1, PLAN_HISTORY_MAX_ENTRIES);
    let index: Vec<(i64, i64)> = {
        let mut statement = tx.prepare_cached(
            "SELECT revision, event_id FROM conversation_plan_history \
             WHERE plan_id = ?1 AND revision > ?2 ORDER BY revision LIMIT ?3",
        )?;
        statement
            .query_map(
                params![
                    row.plan_id,
                    to_i64(after_revision.unwrap_or(0), "plan revision")?,
                    i64::from(limit) + 1
                ],
                |entry| Ok((entry.get(0)?, entry.get(1)?)),
            )?
            .collect::<Result<_, _>>()?
    };
    let more = index.len() > limit as usize;
    let mut entries = Vec::new();
    for (revision, event_id) in index.into_iter().take(limit as usize) {
        let mut statement = tx.prepare_cached(
            "SELECT id, session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload, \
             body_format FROM events WHERE id = ?1",
        )?;
        let mut found = statement.query([event_id])?;
        let (event, updated_at_unix_ms) = match found.next()? {
            Some(event_row) => match row_to_event(&tx, event_row)?.event.kind {
                EventKind::ConversationPlan(event) => (
                    Some(event),
                    Some(event_row.get::<_, i64>(4)?.div_euclid(1_000_000)),
                ),
                _ => (None, None),
            },
            None => (None, None),
        };
        entries.push(PlanHistoryEntry {
            revision: to_u64(revision, "plan revision")?,
            event_id,
            event,
            updated_at_unix_ms,
        });
    }
    let next_after_revision = more
        .then(|| entries.last().map(|entry| entry.revision))
        .flatten();
    Ok(PlanHistoryPage {
        plan_id: Some(row.plan_id),
        revision: row.revision,
        entries,
        next_after_revision,
    })
}
