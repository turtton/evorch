//! Absolute counters, independent of the goal's movable semantic binding.
use crate::StorageError;
use event_bus::ThreadGoalUsage;
use rusqlite::{Connection, params, types::ValueRef};

fn counter(row: &rusqlite::Row<'_>, index: usize) -> Result<u64, StorageError> {
    let ValueRef::Blob(bytes) = row.get_ref(index)? else {
        return Err(StorageError::Serialization(
            "goal usage counter is not a blob".into(),
        ));
    };
    let bytes: [u8; 8] = bytes
        .try_into()
        .map_err(|_| StorageError::Serialization("goal usage counter is not eight bytes".into()))?;
    Ok(u64::from_be_bytes(bytes))
}

pub(crate) fn get(conn: &Connection, id: &str) -> Result<Option<ThreadGoalUsage>, StorageError> {
    let mut statement = conn.prepare_cached("SELECT model_requests, input_tokens, output_tokens FROM thread_goal_usage_current WHERE goal_id = ?1")?;
    let mut rows = statement.query([id])?;
    rows.next()?
        .map(|row| {
            Ok(ThreadGoalUsage {
                model_requests: u32::try_from(counter(row, 0)?).map_err(|_| {
                    StorageError::Serialization("goal request counter exceeds u32".into())
                })?,
                input_tokens: counter(row, 1)?,
                output_tokens: counter(row, 2)?,
            })
        })
        .transpose()
}

pub(crate) fn upsert(
    conn: &Connection,
    id: &str,
    thread: &str,
    root: &str,
    usage: &ThreadGoalUsage,
) -> Result<(), StorageError> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let thread_deleted: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM thread_tombstones WHERE thread_id = ?1)",
        [thread],
        |r| r.get(0),
    )?;
    let run_deleted: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id = ?1)",
        [root],
        |r| r.get(0),
    )?;
    if thread_deleted || run_deleted {
        return Err(StorageError::Tombstoned {
            thread_id: thread_deleted.then(|| thread.to_owned()),
            run_id: run_deleted.then(|| root.to_owned()),
        });
    }
    // Never repair a corrupt row by overwriting it with smaller in-memory values.
    let _ = get(&tx, id)?;
    tx.execute("INSERT INTO thread_goal_usage_current (goal_id, model_requests, input_tokens, output_tokens) VALUES (?1, ?2, ?3, ?4)
        ON CONFLICT(goal_id) DO UPDATE SET
        model_requests = max(model_requests, excluded.model_requests),
        input_tokens = max(input_tokens, excluded.input_tokens),
        output_tokens = max(output_tokens, excluded.output_tokens)",
        params![id, u64::from(usage.model_requests).to_be_bytes().as_slice(), usage.input_tokens.to_be_bytes().as_slice(), usage.output_tokens.to_be_bytes().as_slice()])?;
    #[cfg(feature = "goal-usage-test-support")]
    crate::goal_usage_test_support::visit(id, crate::goal_usage_test_support::Point::BeforeCommit)?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests;
