//! Read-only diagnosis queries for out-of-process investigators (`evorch inspect`).
//!
//! Rows are returned as JSON so investigators see stored columns without depending on
//! typed decoders that may reject older payloads. Persisted secrets are already redacted
//! or rejected at ingress; nothing here un-redacts.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, params_from_iter};
use serde_json::{Map, Value, json};

use crate::improvement::{ImprovementCandidate, ImprovementStatus};
use crate::{Database, StorageError};

/// Columns holding JSON text that are expanded in inspection output.
const JSON_COLUMNS: [&str; 6] = [
    "payload",
    "config_json",
    "checkpoints_json",
    "messages_json",
    "recent_run_ids",
    "body",
];

/// A stored event with its payload kept as raw JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct InspectEvent {
    pub id: i64,
    pub session_id: Option<String>,
    pub wall_clock_ns: i64,
    pub kind: String,
    pub payload: Value,
}

/// Which events to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventScope<'a> {
    /// Events whose payload names this run in any `run_id` field.
    Run(&'a str),
    /// Events persisted in the inclusive wall-clock window.
    Window { from_ns: i64, to_ns: i64 },
}

impl Database {
    /// Opens an existing database without creating, migrating or writing it.
    ///
    /// # Errors
    /// Fails unless the file exists and is at exactly the current schema version; an older
    /// file must first be opened (and so migrated) by the current evorch.
    pub fn open_read_only(path: &Path) -> Result<Self, StorageError> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_millis(5_000))?;
        let found = conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))?;
        let supported = u32::try_from(crate::migrations::MIGRATIONS.len())
            .map_err(|_| StorageError::OutOfRange("migration count"))?;
        if found > supported {
            return Err(StorageError::SchemaTooNew { found, supported });
        }
        if found < supported {
            return Err(StorageError::Serialization(format!(
                "schema version {found} is older than {supported}; open it once with the current evorch to migrate"
            )));
        }
        Ok(Self { conn })
    }

    /// Up to `limit` most recent matching events in persistence order, and whether older
    /// matches were left out.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn inspect_events(
        &self,
        scope: EventScope<'_>,
        limit: usize,
    ) -> Result<(Vec<InspectEvent>, bool), StorageError> {
        let limit = limit.clamp(1, 10_000);
        let mut events = Vec::new();
        let mut truncated = false;
        match scope {
            EventScope::Run(run_id) => {
                // The leading quote keeps `"parent_run_id"` from matching; the JSON check
                // below drops matches inside string values.
                let needle = format!("\"run_id\":{}", Value::String(run_id.into()));
                let mut statement = self.conn.prepare(
                    "SELECT id, session_id, wall_clock_ns, kind, payload FROM events
                     WHERE instr(payload, ?1) > 0 ORDER BY id DESC",
                )?;
                let mut rows = statement.query([needle])?;
                while let Some(row) = rows.next()? {
                    let event = event_row(row)?;
                    if !names_run(&event.payload, run_id) {
                        continue;
                    }
                    if events.len() == limit {
                        truncated = true;
                        break;
                    }
                    events.push(event);
                }
            }
            EventScope::Window { from_ns, to_ns } => {
                let mut statement = self.conn.prepare(
                    "SELECT id, session_id, wall_clock_ns, kind, payload FROM events
                     WHERE wall_clock_ns BETWEEN ?1 AND ?2 ORDER BY id DESC LIMIT ?3",
                )?;
                let mut rows = statement.query(rusqlite::params![
                    from_ns,
                    to_ns,
                    i64::try_from(limit + 1).unwrap_or(i64::MAX)
                ])?;
                while let Some(row) = rows.next()? {
                    if events.len() == limit {
                        truncated = true;
                        break;
                    }
                    events.push(event_row(row)?);
                }
            }
        }
        events.reverse();
        Ok((events, truncated))
    }

    /// Everything persisted about one run except its events: context snapshot, children,
    /// run ledger, provider requests, user questions and improvement candidates naming it.
    /// Messages and checkpoints are summarized unless `full` is set.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn inspect_run(&self, run_id: &str, full: bool) -> Result<Value, StorageError> {
        let mut context = self
            .json_rows("SELECT * FROM run_contexts WHERE run_id = ?1", &[run_id])?
            .into_iter()
            .next();
        if let Some(Value::Object(object)) = context.as_mut()
            && !full
        {
            for column in ["messages_json", "checkpoints_json"] {
                if let Some(value) = object.remove(column) {
                    let count = value.as_array().map_or(0, Vec::len);
                    object.insert(column.replace("_json", "_count"), json!(count));
                }
            }
        }
        let children: Vec<Value> = self
            .json_rows(
                "SELECT run_id, role, name, terminal_phase, updated_at_ns FROM run_contexts
                 WHERE parent_run_id = ?1 ORDER BY updated_at_ns",
                &[run_id],
            )?
            .into_iter()
            .collect();
        let mut event_kinds = BTreeMap::<String, u64>::new();
        let (events, _) = self.inspect_events(EventScope::Run(run_id), 10_000)?;
        let mut diagnostics = Vec::new();
        for event in events {
            *event_kinds.entry(event.kind.clone()).or_default() += 1;
            if event.kind == "Diagnostic" {
                diagnostics.push(event_json(&event));
            }
        }
        let candidate_pattern = format!("%{}%", Value::String(run_id.into()));
        Ok(json!({
            "run_id": run_id,
            "context": context,
            "children": children,
            "ledger": self.json_rows("SELECT * FROM run_ledger WHERE run_id = ?1 ORDER BY seq", &[run_id])?,
            "usage_requests": self.json_rows("SELECT * FROM usage_requests WHERE run_id = ?1 ORDER BY at_ns", &[run_id])?,
            "user_questions": self.json_rows(
                "SELECT * FROM user_questions WHERE run_id = ?1
                 OR id IN (SELECT question_id FROM user_question_links WHERE run_id = ?1)",
                &[run_id],
            )?,
            "event_kinds": event_kinds,
            "diagnostics": diagnostics,
            "improvement_candidates": self.json_rows(
                "SELECT candidate_id, project, code, title, status, occurrences FROM improvement_candidates
                 WHERE run_id = ?1 OR recent_run_ids LIKE ?2 ORDER BY created_at_ns",
                &[run_id, &candidate_pattern],
            )?,
        }))
    }

    /// Newest candidates first across every project unless one is named; limit is capped at 1000.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn inspect_candidates(
        &self,
        project: Option<&str>,
        status: Option<ImprovementStatus>,
        limit: usize,
    ) -> Result<Vec<ImprovementCandidate>, StorageError> {
        let projects: Vec<String> = match project {
            Some(project) => vec![project.into()],
            None => self
                .conn
                .prepare("SELECT DISTINCT project FROM improvement_candidates ORDER BY project")?
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?,
        };
        let mut candidates = Vec::new();
        for project in projects {
            candidates.extend(self.improvement_candidates(&project, status, limit)?);
        }
        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.created_at_ns));
        candidates.truncate(limit.min(1000));
        Ok(candidates)
    }

    fn json_rows(&self, sql: &str, params: &[&str]) -> Result<Vec<Value>, StorageError> {
        let mut statement = self.conn.prepare(sql)?;
        let names: Vec<String> = statement
            .column_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut rows = statement.query(params_from_iter(params))?;
        let mut values = Vec::new();
        while let Some(row) = rows.next()? {
            let mut object = Map::new();
            for (index, name) in names.iter().enumerate() {
                object.insert(name.clone(), column_json(name, row.get_ref(index)?));
            }
            values.push(Value::Object(object));
        }
        Ok(values)
    }
}

/// JSON form of an event, as `evorch inspect` prints it.
pub fn event_json(event: &InspectEvent) -> Value {
    json!({
        "id": event.id,
        "session_id": event.session_id,
        "wall_clock_ns": event.wall_clock_ns,
        "kind": event.kind,
        "payload": event.payload,
    })
}

fn event_row(row: &rusqlite::Row<'_>) -> Result<InspectEvent, StorageError> {
    let payload: String = row.get(4)?;
    Ok(InspectEvent {
        id: row.get(0)?,
        session_id: row.get(1)?,
        wall_clock_ns: row.get(2)?,
        kind: row.get(3)?,
        payload: serde_json::from_str(&payload).unwrap_or(Value::String(payload)),
    })
}

fn names_run(value: &Value, run_id: &str) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            (key == "run_id" && value.as_str() == Some(run_id)) || names_run(value, run_id)
        }),
        Value::Array(items) => items.iter().any(|item| names_run(item, run_id)),
        _ => false,
    }
}

fn column_json(name: &str, value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(number) => json!(number),
        ValueRef::Real(number) => json!(number),
        ValueRef::Text(bytes) => {
            let text = String::from_utf8_lossy(bytes);
            JSON_COLUMNS
                .contains(&name)
                .then(|| serde_json::from_str(&text).ok())
                .flatten()
                .unwrap_or_else(|| Value::String(text.into_owned()))
        }
        ValueRef::Blob(bytes) => json!({ "blob_bytes": bytes.len() }),
    }
}
