//! Compare exact production replay in isolated in-memory databases before deletion.
//! One task per tick; oversized/unknown histories fail closed rather than blocking writers.

use std::path::Path;

use event_bus::{EventKind, OrchestratorEvent};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use crate::durable_tasks::DurableTasksModel;
use crate::entity::TaskStatus;
use crate::repo::event::{StoredEvent, row_to_event};
use crate::{Database, StorageError};

const MAX_ROWS: usize = 256;
const MAX_BYTES: u64 = 16 * 1024 * 1024;
const TASK_ROWS: &str = "kind = 'Orchestrator' AND json_valid(payload) AND json_extract(payload, '$.payload.kind') IN ('TaskProgressed', 'TaskCheckpoint')";

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct RetentionSummary {
    pub tasks_examined: u64,
    pub tasks_eligible: u64,
    pub tasks_excluded: u64,
    pub event_count: u64,
    pub payload_bytes: u64,
}

#[derive(Debug, Default)]
pub(crate) struct Cursor {
    after: i64,
    through: Option<i64>,
}

struct History {
    events: Vec<StoredEvent>,
    sizes: Vec<u64>,
}

#[derive(Default)]
struct PreviewHistory {
    sizes: Vec<(i64, u64)>,
    orchestrator_rows: usize,
    lifecycle_rows: usize,
    orchestrator_bytes: u64,
    lifecycle_bytes: u64,
}

impl PreviewHistory {
    fn bounded(&self) -> bool {
        self.orchestrator_rows <= MAX_ROWS
            && self.lifecycle_rows <= MAX_ROWS
            && self.orchestrator_bytes <= MAX_BYTES
            && self.lifecycle_bytes <= MAX_BYTES
    }
}

/// A finite read-only snapshot; no migrations, deletes, checkpoints or vacuum.
pub fn preview_task_progress_retention(path: &Path) -> Result<RetentionSummary, StorageError> {
    let _lease = super::offline::preview_lease(path)?;
    let (conn, preview_files) = super::offline::read_only_preview_connection(path)?;
    let tx = conn.unchecked_transaction()?;
    // Build a small in-memory ID/size index in ONE source scan. The live
    // store may predate v26; never create its indexes as a preview side effect.
    let mut by_task: std::collections::BTreeMap<String, PreviewHistory> = Default::default();
    let mut statement = tx.prepare("SELECT id, json_extract(payload, '$.payload.payload.task_id'), kind, OCTET_LENGTH(payload) FROM events WHERE kind IN ('Orchestrator','Lifecycle') AND json_valid(payload) AND json_type(payload, '$.payload.payload.task_id') = 'text' AND (kind = 'Lifecycle' OR json_extract(payload, '$.payload.kind') IN ('TaskProgressed','TaskCheckpoint')) ORDER BY id")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let task: String = row.get(1)?;
        let lifecycle = row.get::<_, String>(2)? == "Lifecycle";
        let bytes = row.get::<_, i64>(3)? as u64;
        let entry = by_task.entry(task).or_default();
        if lifecycle {
            entry.lifecycle_rows += 1;
            entry.lifecycle_bytes += bytes;
        } else {
            entry.orchestrator_rows += 1;
            entry.orchestrator_bytes += bytes;
        }
        if entry.bounded() {
            entry.sizes.push((id, bytes));
        }
    }
    drop(rows);
    drop(statement);
    let mut summary = RetentionSummary::default();
    for history in by_task.into_values() {
        if history.orchestrator_rows == 0 {
            continue;
        }
        summary.tasks_examined += 1;
        summary.tasks_excluded += 1;
        if !history.bounded() {
            continue;
        }
        if let Some(history) = decode_history(&tx, history.sizes)?
            && let Some(keep) = equivalent(&history)?
        {
            summary.tasks_excluded -= 1;
            summary.tasks_eligible += 1;
            for (event, bytes) in history.events.iter().zip(history.sizes) {
                if is_progress(event) && !keep.contains(&event.id) {
                    summary.event_count += 1;
                    summary.payload_bytes += bytes;
                }
            }
        }
    }
    tx.commit()?;
    preview_files.verify()?;
    Ok(summary)
}

/// Serialize eligibility and deletion with every application writer, including
/// other processes. Each commit leaves an independently replay-equivalent history.
pub(crate) fn tick(
    conn: &Connection,
    cursor: &mut Cursor,
) -> Result<RetentionSummary, StorageError> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let (summary, _) = step(&tx, cursor, true)?;
    tx.commit()?;
    Ok(summary)
}

fn step(
    conn: &Connection,
    cursor: &mut Cursor,
    delete: bool,
) -> Result<(RetentionSummary, bool), StorageError> {
    let through = match cursor.through {
        Some(through) => through,
        None => {
            let through = conn.query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |row| {
                row.get(0)
            })?;
            cursor.through = Some(through);
            through
        }
    };
    // The ID-leading partial index makes this a bounded range seek, not a
    // task-name scan that can inspect arbitrarily many post-pass arrivals.
    // Inspect at most one index entry, advancing even for malformed/excluded
    // rows. Revisiting a task is harmless and keeps discovery stateless/bounded.
    let candidate: Option<(i64, rusqlite::types::Value)> = conn.query_row(
        &format!("SELECT id, json_extract(payload, '$.payload.payload.task_id') FROM events INDEXED BY idx_events_durable_retention_scan WHERE {TASK_ROWS} AND id > ?1 AND id <= ?2 ORDER BY id LIMIT 1"),
        params![cursor.after, through], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?;
    let Some((id, task)) = candidate else {
        *cursor = Cursor::default();
        return Ok((RetentionSummary::default(), true));
    };
    cursor.after = id;
    let rusqlite::types::Value::Text(task) = task else {
        return Ok((RetentionSummary::default(), false));
    };
    let mut summary = RetentionSummary {
        tasks_examined: 1,
        tasks_excluded: 1,
        ..Default::default()
    };
    // Ordinary append does not maintain `tasks`. Its stored projection is not
    // an eligibility source; equivalent() derives terminal state with the same
    // bounded production replay used to prove full/reduced equality.
    if let Some(history) = history(conn, &task, through)?
        && let Some(keep) = equivalent(&history)?
    {
        summary.tasks_excluded = 0;
        summary.tasks_eligible = 1;
        for (stored, bytes) in history.events.iter().zip(&history.sizes) {
            if is_progress(stored) && !keep.contains(&stored.id) {
                if delete {
                    conn.execute("DELETE FROM events WHERE id = ?1", [stored.id])?;
                    if let Some(session) = &stored.session_id {
                        conn.execute("UPDATE sessions SET total_event_bytes = MAX(0, total_event_bytes - ?1) WHERE id = ?2", params![*bytes as i64, session])?;
                    }
                }
                summary.event_count += 1;
                summary.payload_bytes += bytes;
            }
        }
    }
    Ok((summary, false))
}

use rusqlite::OptionalExtension;

fn history(conn: &Connection, task: &str, through: i64) -> Result<Option<History>, StorageError> {
    let sql = format!(
        "SELECT id, OCTET_LENGTH(payload) FROM events WHERE ({TASK_ROWS}) AND json_extract(payload, '$.payload.payload.task_id') = ?1 ORDER BY id LIMIT ?2"
    );
    let mut statement = conn.prepare(&sql)?;
    let mut sizes = statement
        .query_map(params![task, (MAX_ROWS + 1) as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)? as u64))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if sizes.len() > MAX_ROWS
        || sizes.iter().map(|(_, bytes)| bytes).sum::<u64>() > MAX_BYTES
        || sizes.iter().any(|(id, _)| *id > through)
    {
        return Ok(None);
    }
    // Lifecycle events can establish creation time/session attribution. Include
    // them in both replays; never delete them. Cap them independently too.
    let mut statement = conn.prepare("SELECT id, OCTET_LENGTH(payload) FROM events WHERE kind = 'Lifecycle' AND json_valid(payload) AND json_extract(payload, '$.payload.payload.task_id') = ?1 ORDER BY id LIMIT ?2")?;
    let lifecycle = statement
        .query_map(params![task, (MAX_ROWS + 1) as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)? as u64))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if lifecycle.len() > MAX_ROWS
        || lifecycle.iter().map(|(_, bytes)| bytes).sum::<u64>() > MAX_BYTES
    {
        return Ok(None);
    }
    sizes.extend(lifecycle);
    sizes.sort_unstable_by_key(|(id, _)| *id);
    decode_history(conn, sizes)
}

fn decode_history(
    conn: &Connection,
    sizes: Vec<(i64, u64)>,
) -> Result<Option<History>, StorageError> {
    let mut events = Vec::new();
    for (id, _) in &sizes {
        let decoded = conn.query_row("SELECT id,session_id,schema_version,monotonic_ns,wall_clock_ns,kind,payload FROM events WHERE id = ?1", [id], |row| Ok(row_to_event(row)))?;
        match decoded {
            Ok(event) => events.push(event),
            Err(_) => return Ok(None),
        }
    }
    Ok(Some(History {
        events,
        sizes: sizes.into_iter().map(|(_, bytes)| bytes).collect(),
    }))
}

fn is_progress(event: &StoredEvent) -> bool {
    matches!(
        event.event.kind,
        EventKind::Orchestrator(OrchestratorEvent::TaskProgressed { .. })
    )
}

fn equivalent(history: &History) -> Result<Option<Vec<i64>>, StorageError> {
    let progress: Vec<_> = history
        .events
        .iter()
        .filter(|event| is_progress(event))
        .collect();
    let Some(last) = progress.last() else {
        return Ok(None);
    };
    // No progress of any generation may follow the terminal boundary. Unknown
    // statuses, Failed and Stopped are never treated as terminal.
    let EventKind::Orchestrator(OrchestratorEvent::TaskProgressed {
        progress: value,
        task_id,
        ..
    }) = &last.event.kind
    else {
        unreachable!()
    };
    let Ok(value) = serde_json::from_value::<crate::entity::TaskContinuation>(value.clone()) else {
        return Ok(None);
    };
    if !matches!(value.status, TaskStatus::Completed | TaskStatus::Cancelled) {
        return Ok(None);
    }
    let first = progress.iter().find(|event| {
        matches!(&event.event.kind, EventKind::Orchestrator(OrchestratorEvent::TaskProgressed { progress, .. }) if progress.get("input").is_some_and(|input| !input.is_null()))
    });
    let mut keep = vec![last.id];
    if let Some(first) = first {
        keep.push(first.id)
    }
    let full = replay(&history.events, task_id)?;
    let reduced: Vec<_> = history
        .events
        .iter()
        .filter(|event| !is_progress(event) || keep.contains(&event.id))
        .cloned()
        .collect();
    let retained = replay(&reduced, task_id)?;
    if full != retained
        || !full.0.as_ref().is_some_and(|task| {
            matches!(task.status, TaskStatus::Completed | TaskStatus::Cancelled)
        })
    {
        return Ok(None);
    }
    // A terminal fence may precede the last row even if that row is also terminal.
    // Do not remove a fence while keeping late resume/retry/heartbeat progress.
    if progress.iter().take(progress.len().saturating_sub(1)).any(|event| {
        matches!(&event.event.kind, EventKind::Orchestrator(OrchestratorEvent::TaskProgressed { progress, .. }) if progress.get("status").and_then(serde_json::Value::as_str).is_some_and(|status| matches!(status, "completed" | "cancelled")))
    }) { return Ok(None) }
    Ok(Some(keep))
}

fn replay(
    events: &[StoredEvent],
    task: &str,
) -> Result<(Option<crate::entity::TaskRecord>, DurableTasksModel), StorageError> {
    let db = Database::open_in_memory()?;
    let mut gui = DurableTasksModel::default();
    for stored in events {
        if let Some(session) = &stored.session_id {
            db.conn.execute("INSERT OR IGNORE INTO sessions(id,status,created_at_ns,updated_at_ns) VALUES (?1,'running',0,0)", [session])?;
        }
        db.conn.execute("INSERT INTO events(id,session_id,schema_version,monotonic_ns,wall_clock_ns,kind,payload) VALUES (?1,?2,?3,?4,?5,?6,?7)", params![stored.id,stored.session_id,stored.event.meta.schema_version,stored.event.meta.monotonic.as_nanos() as i64,crate::system_time_to_ns(stored.event.meta.wall_clock)?, if is_progress(stored) || matches!(stored.event.kind, EventKind::Orchestrator(_)) { "Orchestrator" } else { "Lifecycle" },serde_json::to_string(&stored.event.kind).map_err(|error| super::invalid(error.to_string()))?])?;
        gui.apply_event(&stored.event);
    }
    crate::projection::reconcile(&db.conn)?;
    Ok((crate::repo::task::get(&db.conn, task)?, gui))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_has_a_vm_budget_and_yields_after_many_post_pass_arrivals() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::StorageConfig {
            db_path: dir.path().join("events.db"),
            retain_terminal_task_progress: false,
            ..Default::default()
        };
        let writer = crate::Storage::open(config.clone()).unwrap();
        let db = Database::open(&config).unwrap();
        for task in [serde_json::json!(42), serde_json::json!("z-old")] {
            db.conn
                .execute(
                    "INSERT INTO events(schema_version,monotonic_ns,wall_clock_ns,kind,payload) VALUES(1,0,0,'Orchestrator',?1)",
                    [serde_json::json!({"kind":"Orchestrator","payload":{"kind":"TaskProgressed","payload":{"task_id":task,"run_id":"run","progress":{"status":"running"},"reason":"boundary"}}}).to_string()],
                )
                .unwrap();
        }
        let mut cursor = Cursor::default();
        tick(&db.conn, &mut cursor).unwrap();
        assert_eq!(cursor.after, 1, "malformed task must still advance");
        assert_eq!(cursor.through, Some(2));
        db.conn.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<20000) INSERT INTO events(schema_version,monotonic_ns,wall_clock_ns,kind,payload) SELECT 1,0,0,'Orchestrator',json_object('kind','Orchestrator','payload',json_object('kind','TaskProgressed','payload',json_object('task_id',printf('a-new-%05d',x),'run_id','run','progress',json_object('status','running'),'reason','boundary'))) FROM n;").unwrap();
        for expected_after in [2, 0] {
            let budget = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let observed = budget.clone();
            db.conn
                .progress_handler(
                    100,
                    Some(move || {
                        observed.fetch_add(100, std::sync::atomic::Ordering::Relaxed) >= 10_000
                    }),
                )
                .unwrap();
            tick(&db.conn, &mut cursor).unwrap();
            db.conn.progress_handler(0, None::<fn() -> bool>).unwrap();
            assert!(budget.load(std::sync::atomic::Ordering::Relaxed) < 10_000);
            assert_eq!(cursor.after, expected_after);
            // Completion means the IMMEDIATE transaction has yielded; a
            // subsequent command on the real writer must be serviced.
            writer
                .handle()
                .append_event(
                    None,
                    &event_bus::Event::new(event_bus::LifecycleEvent::Started {
                        session_id: "other-writer".into(),
                    }),
                )
                .unwrap();
        }
        assert!(cursor.through.is_none(), "finite pass must finish");
        writer.close();
    }
}
