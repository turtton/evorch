//! One bounded, restartable projection step. Scheduling is deliberately separate.
use std::time::Duration;

use rusqlite::{Transaction, TransactionBehavior, params};

use super::index::{self, State};
use crate::{Database, StorageError};

/// Source and derived work admitted by one construction step.
#[derive(Debug, Clone, Copy)]
pub struct BuildBudget {
    pub rows: usize,
    pub bytes: usize,
    pub writes: usize,
    pub elapsed: Duration,
}
impl Default for BuildBudget {
    fn default() -> Self {
        Self {
            rows: 8192,
            bytes: 32 * 1024 * 1024,
            writes: 16 * 1024,
            elapsed: Duration::from_millis(10),
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct BuildProgress {
    pub rows: usize,
    pub bytes: usize,
    pub writes: usize,
    pub ready: bool,
    pub conflicted: bool,
}

/// Injectable for budget boundaries, debounce and retry decisions; never sleeps.
pub trait Clock {
    fn now_ns(&self) -> i64;
}
pub struct SystemClock;
impl Clock for SystemClock {
    fn now_ns(&self) -> i64 {
        crate::system_time_to_ns(std::time::SystemTime::now()).unwrap_or(i64::MAX)
    }
}

pub(crate) const TABLES: [&str; 5] = [
    "history_message_sources",
    "history_pending_parts",
    "history_tool_starts",
    "history_messages",
    "history_event_cuts",
];

fn live(conn: &rusqlite::Connection, run: &str) -> Result<bool, StorageError> {
    Ok(conn.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=?1)",
        [run],
        |r| r.get(0),
    )?)
}

pub(crate) fn start(
    conn: &rusqlite::Connection,
    run: &str,
    budget: BuildBudget,
) -> Result<BuildProgress, StorageError> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let mut progress = BuildProgress::default();
    if !index::version_ok(&tx)? || !live(&tx, run)? {
        return Ok(progress);
    }
    tx.execute(
        "INSERT OR IGNORE INTO history_run_state(run_id) VALUES (?1)",
        [run],
    )?;
    let state = index::state(&tx, run)?.ok_or(StorageError::Io("history state absent".into()))?;
    if state.generation.is_none() || state.source.is_none() {
        return Ok(progress);
    }
    if state.phase == 0 {
        for table in TABLES {
            let remaining = budget.writes.saturating_sub(progress.writes);
            if remaining == 0 {
                tx.commit()?;
                return Ok(progress);
            }
            let deleted = tx.execute(
                &format!(
                    "DELETE FROM {table} WHERE rowid IN \
                     (SELECT rowid FROM {table} WHERE run_id=?1 LIMIT ?2)"
                ),
                params![run, remaining as u64],
            )?;
            progress.writes += deleted;
            if deleted == remaining {
                tx.commit()?;
                return Ok(progress);
            }
        }
        // Starting a replacement is itself a new incarnation. Never reuse an old
        // generation, even if a different process cleaned the previous rows.
        tx.execute(
            "UPDATE history_run_state SET phase=1, checkpoint=NULL, output_count=0, \
             merge_max=NULL, first_prompt_id=NULL, first_prompt_merge_max=NULL, \
             reducer=?2, generation=CASE WHEN generation < 9223372036854775807 \
                 THEN generation+1 END WHERE run_id=?1",
            params![
                run,
                serde_json::to_string(&super::Reducer::default())
                    .map_err(|e| StorageError::Serialization(e.to_string()))?
            ],
        )?;
        progress.writes += 1;
    } else if state.phase == 2 {
        if state.source == state.applied {
            progress.ready = true;
        } else {
            // A hook-free tail or failed hook resumes from the durable cut.
            tx.execute(
                "UPDATE history_run_state SET phase=1 WHERE run_id=?1",
                [run],
            )?;
            progress.writes += 1;
        }
    }
    tx.commit()?;
    Ok(progress)
}

fn same(state: &State, current: &State) -> bool {
    state.generation == current.generation
        && state.source == current.source
        && state.checkpoint == current.checkpoint
        && state.phase == current.phase
}

/// Decode and reduce under a reader snapshot, then release it before taking the
/// write lock. `reader` and `writer` may be different processes' connections.
pub fn step(
    reader: &Database,
    writer: &Database,
    run: &str,
    budget: BuildBudget,
    clock: &impl Clock,
) -> Result<BuildProgress, StorageError> {
    if budget.rows == 0 || budget.writes == 0 || budget.bytes == 0 {
        return Ok(BuildProgress::default());
    }
    let progress = start(&writer.conn, run, budget)?;
    if progress.ready || progress.writes >= budget.writes {
        return Ok(progress);
    }
    let Some(batch) = prepare(reader, run, budget, clock, progress)? else {
        return Ok(progress);
    };
    apply(&writer.conn, batch)
}

pub(crate) struct PreparedStep {
    prior: State,
    checkpoint: Option<i64>,
    prepared: Option<index::Prepared>,
    exhausted: bool,
    progress: BuildProgress,
}

pub(crate) fn prepare(
    reader: &Database,
    run: &str,
    budget: BuildBudget,
    clock: &impl Clock,
    mut progress: BuildProgress,
) -> Result<Option<PreparedStep>, StorageError> {
    let start_at = clock.now_ns();
    let snapshot = reader.conn.unchecked_transaction()?;
    if !index::version_ok(&snapshot)? || !live(&snapshot, run)? {
        return Ok(None);
    }
    let Some(prior) = index::state(&snapshot, run)? else {
        return Ok(None);
    };
    if prior.phase != 1 || prior.generation.is_none() || prior.source.is_none() {
        return Ok(None);
    }
    let mut checkpoint = prior.checkpoint;
    let mut prepared = None;
    let mut exhausted = false;
    {
        let mut scan = snapshot.prepare_cached(
            if checkpoint.is_some() {
                "SELECT id, wall_clock_ns, kind, subkind, octet_length(payload) FROM events WHERE run_id=?1 AND id>?2 ORDER BY id LIMIT ?3"
            } else {
                "SELECT id, wall_clock_ns, kind, subkind, octet_length(payload) FROM events WHERE run_id=?1 AND ?2 IS NULL ORDER BY id LIMIT ?3"
            },
        )?;
        let mut rows = scan.query(params![run, checkpoint, budget.rows as u64])?;
        loop {
            let Some(row) = rows.next()? else {
                exhausted = true;
                break;
            };
            let bytes: usize = row.get(4)?;
            if progress.bytes.saturating_add(bytes) > budget.bytes {
                if progress.rows == 0 {
                    return Err(StorageError::Io(
                        "history source exceeds build budget".into(),
                    ));
                }
                break;
            }
            let id = row.get(0)?;
            let kind: String = row.get(2)?;
            let subkind: Option<String> = row.get(3)?;
            if subkind.is_none() {
                return Err(StorageError::Io("unbackfilled history source".into()));
            }
            progress.rows += 1;
            progress.bytes += bytes;
            checkpoint = Some(id);
            if crate::COMMITTED_SUBKINDS
                .contains(&(kind.as_str(), subkind.as_deref().unwrap_or("")))
            {
                progress.bytes -= bytes;
                let mut remaining = budget.bytes - progress.bytes;
                let (kind, tokenized) = index::source_kind_limited(&snapshot, id, &mut remaining)?;
                let at: i64 = row.get(1)?;
                let p = index::prepare_budgeted(
                    &snapshot,
                    prior.clone(),
                    id,
                    at.max(-i64::MAX),
                    &kind,
                    tokenized,
                    &mut remaining,
                    budget.rows - progress.rows,
                )?;
                progress.bytes = budget.bytes - remaining;
                progress.rows += p.sources.len().saturating_sub(1);
                if progress.rows > budget.rows {
                    return Err(StorageError::Io(
                        "history message exceeds source row budget".into(),
                    ));
                }
                if p.write_bound > budget.writes - progress.writes {
                    // Cleanup may have consumed this step's write allowance.
                    // Its checkpoint is durable; retry source preparation with a
                    // fresh budget rather than treating a partial tick as corrupt.
                    if progress.writes > 0 {
                        return Ok(None);
                    }
                    return Err(StorageError::Io(
                        "history message exceeds write budget".into(),
                    ));
                }
                prepared = Some(p);
                break;
            }
            if clock.now_ns().saturating_sub(start_at)
                >= i64::try_from(budget.elapsed.as_nanos()).unwrap_or(i64::MAX)
                || progress.rows >= budget.rows.saturating_sub(4095).max(1)
                || progress.bytes >= budget.bytes.saturating_sub(16 * 1024 * 1024).max(1)
            {
                break;
            }
        }
    }
    snapshot.commit()?;

    Ok(Some(PreparedStep {
        prior,
        checkpoint,
        prepared,
        exhausted,
        progress,
    }))
}

pub(crate) fn apply(
    conn: &rusqlite::Connection,
    batch: PreparedStep,
) -> Result<BuildProgress, StorageError> {
    let PreparedStep {
        prior,
        checkpoint,
        prepared,
        exhausted,
        mut progress,
    } = batch;
    let run = &prior.run;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let current = index::state(&tx, run)?;
    if !index::version_ok(&tx)?
        || !live(&tx, run)?
        || current.as_ref().is_none_or(|s| !same(&prior, s))
    {
        progress.conflicted = true;
        return Ok(progress);
    }
    if let Some(prepared) = prepared {
        let before = tx.total_changes();
        if !index::apply(&tx, &prepared, false)? {
            progress.conflicted = true;
            return Ok(progress);
        }
        progress.writes += (tx.total_changes() - before) as usize;
    } else {
        // No source was skipped: readiness is certified only at an exhausted tail.
        let ready = exhausted && !crate::repo::event::select::has_unbackfilled(&tx)?;
        tx.execute(
            "UPDATE history_run_state SET checkpoint=?2, \
             phase=CASE WHEN ?3 THEN 2 ELSE phase END, \
             applied=CASE WHEN ?3 THEN source ELSE applied END WHERE run_id=?1",
            params![run, checkpoint, ready],
        )?;
        progress.writes += 1;
        progress.ready = ready;
    }
    tx.commit()?;
    Ok(progress)
}

impl Database {
    /// Explicit construction step for deterministic tests and measurement tools.
    #[cfg(any(test, feature = "test-support"))]
    pub fn history_build_step(
        &self,
        run: &str,
        budget: BuildBudget,
        clock: &impl Clock,
    ) -> Result<BuildProgress, StorageError> {
        step(self, self, run, budget, clock)
    }
}
