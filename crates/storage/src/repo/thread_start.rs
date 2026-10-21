//! Insert-only thread start states and their retention.

use rusqlite::{Connection, OptionalExtension, params};

use crate::StorageError;
use crate::thread_start::{
    NotReplayableReason, ThreadStartCapture, ThreadStartSnapshot, ThreadStartState,
    ThreadStartSummary, UntrackedEntry, UntrackedKind,
};

/// States deleted per maintenance tick.
const RETENTION_BATCH: i64 = 64;

/// Returns whether the state was inserted; a thread's first state wins.
pub fn save(conn: &Connection, state: &ThreadStartState) -> Result<bool, StorageError> {
    state.validate()?;
    let state = state.clone().without_secrets();
    let (reason, snapshot) = match &state.capture {
        ThreadStartCapture::Replayable(snapshot) => (None, Some(snapshot)),
        ThreadStartCapture::NotReplayable(reason) => (Some(reason.as_str()), None),
    };
    let inserted = conn.execute(
        "INSERT INTO thread_start_states (thread_id, root_run_id, project_root, repo_prefix, \
         head_commit, head_ref, diff, captured_at_ns, replayable, reason) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) ON CONFLICT(thread_id) DO NOTHING",
        params![
            state.thread_id,
            state.root_run_id,
            state.project_root,
            snapshot.map(|snapshot| &snapshot.repo_prefix),
            snapshot.map(|snapshot| &snapshot.head_commit),
            snapshot.and_then(|snapshot| snapshot.head_ref.as_ref()),
            snapshot.map(|snapshot| &snapshot.diff),
            state.captured_at_ns,
            snapshot.is_some(),
            reason,
        ],
    )? == 1;
    if inserted && let Some(snapshot) = snapshot {
        let mut statement = conn.prepare(
            "INSERT INTO thread_start_untracked (thread_id, path, kind, executable, content) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for entry in &snapshot.untracked {
            statement.execute(params![
                state.thread_id,
                entry.path,
                entry.kind.as_str(),
                entry.executable,
                entry.content,
            ])?;
        }
    }
    Ok(inserted)
}

pub fn get(conn: &Connection, thread_id: &str) -> Result<Option<ThreadStartState>, StorageError> {
    let row = conn
        .query_row(
            "SELECT root_run_id, project_root, repo_prefix, head_commit, diff, captured_at_ns, \
             reason, head_ref FROM thread_start_states WHERE thread_id = ?1",
            [thread_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<Vec<u8>>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                ))
            },
        )
        .optional()?;
    let Some((root_run_id, project_root, prefix, head, diff, captured_at_ns, reason, head_ref)) =
        row
    else {
        return Ok(None);
    };
    let capture = match (reason, prefix, head, diff) {
        (Some(reason), None, None, None) => {
            ThreadStartCapture::NotReplayable(parse_reason(&reason)?)
        }
        (None, Some(repo_prefix), Some(head_commit), Some(diff)) => {
            ThreadStartCapture::Replayable(ThreadStartSnapshot {
                repo_prefix,
                head_commit,
                head_ref,
                diff,
                untracked: untracked(conn, thread_id)?,
            })
        }
        _ => {
            return Err(StorageError::Serialization(
                "thread start state mixes content and a reason".into(),
            ));
        }
    };
    Ok(Some(ThreadStartState {
        thread_id: thread_id.to_owned(),
        root_run_id,
        project_root,
        captured_at_ns,
        capture,
    }))
}

fn untracked(conn: &Connection, thread_id: &str) -> Result<Vec<UntrackedEntry>, StorageError> {
    let mut statement = conn.prepare(
        "SELECT path, kind, executable, content FROM thread_start_untracked \
         WHERE thread_id = ?1 ORDER BY path",
    )?;
    let rows = statement.query_map([thread_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, bool>(2)?,
            row.get::<_, Vec<u8>>(3)?,
        ))
    })?;
    rows.map(|row| {
        let (path, kind, executable, content) = row?;
        let kind = UntrackedKind::parse(&kind).ok_or_else(|| {
            StorageError::Serialization(format!("unknown untracked kind {kind:?}"))
        })?;
        Ok(UntrackedEntry {
            path,
            kind,
            executable,
            content,
        })
    })
    .collect()
}

/// Newest first, without contents.
pub fn list_since(
    conn: &Connection,
    since_ns: i64,
) -> Result<Vec<ThreadStartSummary>, StorageError> {
    let mut statement = conn.prepare(
        "SELECT thread_id, root_run_id, project_root, captured_at_ns, reason \
         FROM thread_start_states WHERE captured_at_ns >= ?1 \
         ORDER BY captured_at_ns DESC, thread_id",
    )?;
    let rows = statement.query_map([since_ns], |row| {
        Ok((
            ThreadStartSummary {
                thread_id: row.get(0)?,
                root_run_id: row.get(1)?,
                project_root: row.get(2)?,
                captured_at_ns: row.get(3)?,
                not_replayable: None,
            },
            row.get::<_, Option<String>>(4)?,
        ))
    })?;
    rows.map(|row| {
        let (mut summary, reason) = row?;
        summary.not_replayable = reason.as_deref().map(parse_reason).transpose()?;
        Ok(summary)
    })
    .collect()
}

/// Delete one bounded batch of states captured before `cutoff_ns`, with their
/// untracked entries. States pinned by an evaluation sample are kept. Returns
/// the number of states deleted.
pub fn delete_before(conn: &Connection, cutoff_ns: i64) -> Result<usize, StorageError> {
    Ok(conn.execute(
        "DELETE FROM thread_start_states WHERE thread_id IN (\
         SELECT thread_id FROM thread_start_states WHERE captured_at_ns < ?1 \
         AND thread_id NOT IN (SELECT thread_id FROM eval_samples) \
         ORDER BY captured_at_ns LIMIT ?2)",
        params![cutoff_ns, RETENTION_BATCH],
    )?)
}

fn parse_reason(label: &str) -> Result<NotReplayableReason, StorageError> {
    NotReplayableReason::parse(label).ok_or_else(|| {
        StorageError::Serialization(format!("unknown thread start reason {label:?}"))
    })
}
