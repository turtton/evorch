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
use crate::repo::event::select::{
    IDS_BLANK_MENTIONING_SQL, IDS_BY_RUN_SQL, IDS_UNCONVERTED_MENTIONING_SQL, has_unbackfilled,
    normalize, push_ids,
};
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
    /// Events attributed to this run by their `run_id` column, plus orchestrator
    /// events and unconverted rows whose payload names it in any `run_id` field.
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
                let transaction = self.conn.unchecked_transaction()?;
                let mut candidates = Vec::new();
                push_ids(
                    &transaction,
                    IDS_BY_RUN_SQL,
                    [run_id],
                    false,
                    &mut candidates,
                )?;
                // Task progress names its run in a field the column does not carry.
                push_ids(
                    &transaction,
                    "SELECT id FROM events WHERE kind = 'Orchestrator'",
                    [],
                    true,
                    &mut candidates,
                )?;
                // Any string value equal to the run narrows the payload scan of rows
                // the columns cannot attribute; the checks below decide. Rows whose
                // payload no longer decodes are blank, and are always scanned.
                let needle = Value::String(run_id.into()).to_string();
                push_ids(
                    &transaction,
                    IDS_BLANK_MENTIONING_SQL,
                    [&needle],
                    true,
                    &mut candidates,
                )?;
                if has_unbackfilled(&transaction)? {
                    push_ids(
                        &transaction,
                        IDS_UNCONVERTED_MENTIONING_SQL,
                        [&needle],
                        true,
                        &mut candidates,
                    )?;
                }
                let mut statement = transaction.prepare(
                    "SELECT id, session_id, wall_clock_ns, kind, payload FROM events WHERE id = ?1",
                )?;
                for candidate in normalize(candidates).iter().rev() {
                    let mut rows = statement.query([candidate.id])?;
                    let Some(row) = rows.next()? else { continue };
                    let event = event_row(row)?;
                    if candidate.confirm && !attributed_to_run(&event.payload, run_id) {
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
            .json_rows(
                "SELECT run_id, role, name, parent_run_id, config_json, messages_json, \
                 checkpoints_json, terminal_phase, restorable, updated_at_ns, snapshot_revision, \
                 host_inputs_json FROM run_contexts WHERE run_id = ?1",
                &[run_id],
            )?
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
        let mut manifest_events = Vec::new();
        for event in events {
            *event_kinds.entry(event.kind.clone()).or_default() += 1;
            if event.kind != "Diagnostic" {
                continue;
            }
            if is_review_manifest(&event.payload) {
                manifest_events.push(event);
            } else {
                diagnostics.push(event_json(&event));
            }
        }
        let review_manifests = self.review_manifests_json(&manifest_events)?;
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
            "review_manifests": review_manifests,
            "improvement_candidates": self.json_rows(
                "SELECT candidate_id, project, code, title, status, occurrences FROM improvement_candidates
                 WHERE run_id = ?1 OR recent_run_ids LIKE ?2 ORDER BY created_at_ns",
                &[run_id, &candidate_pattern],
            )?,
        }))
    }

    /// The escalation review records among `events`: each manifest with the
    /// continuation events of its user-request list joined back in, and the
    /// ledger rows of the provider attempts it names. The manifest keeps the
    /// attempts itself; the ledger rows are extra detail while they are retained.
    fn review_manifests_json(&self, events: &[InspectEvent]) -> Result<Vec<Value>, StorageError> {
        let detail = |event: &InspectEvent| -> Option<Value> {
            serde_json::from_str(event.payload["payload"]["detail"].as_str()?).ok()
        };
        struct Part {
            index: u64,
            count: u64,
            refs: Vec<Value>,
        }
        let mut parts = BTreeMap::<String, Vec<Part>>::new();
        for event in events {
            if let Some(part) =
                detail(event).and_then(|detail| detail.get("manifest_part").cloned())
                && let (Some(id), Some(index), Some(count), Some(refs)) = (
                    part["review_id"].as_str(),
                    part["index"].as_u64(),
                    part["count"].as_u64(),
                    part["user_requests"].as_array(),
                )
            {
                parts.entry(id.to_owned()).or_default().push(Part {
                    index,
                    count,
                    refs: refs.clone(),
                });
            }
        }
        let mut manifests = Vec::new();
        for event in events {
            let parsed = detail(event);
            if parsed
                .as_ref()
                .is_some_and(|d| d.get("manifest_part").is_some())
            {
                continue;
            }
            let mut manifest = parsed;
            let mut complete = Value::Null;
            if let Some(manifest) = manifest.as_mut() {
                let id = manifest["review_id"].as_str().map(str::to_owned);
                if let (Some(id), Some(context)) = (id, manifest["context"].as_object_mut()) {
                    let expected = context
                        .get("user_request_parts")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let found = parts.get(&id).map_or(&[][..], Vec::as_slice);
                    // One event per index is joined; a repeated index,
                    // a gap, or a part that disagrees on the count leaves the
                    // list incomplete rather than guessed.
                    let mut by_index = BTreeMap::new();
                    let mut repeated = false;
                    for part in found {
                        repeated |= by_index.insert(part.index, part).is_some();
                    }
                    let consecutive = by_index.keys().copied().eq(0..expected);
                    let counts_agree = found.iter().all(|part| part.count == expected);
                    let count = context.get("user_request_count").and_then(Value::as_u64);
                    if let Some(Value::Array(refs)) = context.get_mut("user_requests") {
                        for part in by_index.values() {
                            refs.extend(part.refs.iter().cloned());
                        }
                        complete = json!(
                            !repeated
                                && consecutive
                                && counts_agree
                                && count == Some(refs.len() as u64)
                        );
                    }
                }
            }
            let request_ids: Vec<String> = manifest
                .as_ref()
                .and_then(|manifest| manifest["attempts"].as_array())
                .map(|attempts| {
                    attempts
                        .iter()
                        .filter_map(|attempt| attempt["request_id"].as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let ledger = if request_ids.is_empty() {
                Vec::new()
            } else {
                let marks = vec!["?"; request_ids.len()].join(",");
                let ids: Vec<&str> = request_ids.iter().map(String::as_str).collect();
                self.json_rows(
                    &format!(
                        "SELECT request_id, at_ns, provider, profile, model, status, failure,
                                input_tokens, output_tokens, cache_read_tokens, cost_usd
                         FROM usage_requests WHERE request_id IN ({marks}) ORDER BY at_ns"
                    ),
                    &ids,
                )?
            };
            let diagnostic = &event.payload["payload"];
            manifests.push(json!({
                "event_id": event.id,
                "wall_clock_ns": event.wall_clock_ns,
                "severity": diagnostic["severity"],
                "thread_id": diagnostic["thread_id"],
                "call_id": diagnostic["call_id"],
                // A record that could not be made, or one this build cannot
                // read, is shown as stored.
                "detail": if manifest.is_some() { Value::Null } else { diagnostic["detail"].clone() },
                "manifest": manifest,
                "user_requests_complete": complete,
                "usage_requests": ledger,
            }));
        }
        Ok(manifests)
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

/// Whether a stored diagnostic is a sandbox escalation review manifest.
fn is_review_manifest(payload: &Value) -> bool {
    let diagnostic = &payload["payload"];
    diagnostic["source"] == "sandbox" && diagnostic["code"] == "escalation_review_manifest"
}

fn event_row(row: &rusqlite::Row<'_>) -> Result<InspectEvent, StorageError> {
    Ok(InspectEvent {
        id: row.get(0)?,
        session_id: row.get(1)?,
        wall_clock_ns: row.get(2)?,
        kind: row.get(3)?,
        payload: column_json("payload", row.get_ref(4)?),
    })
}

/// Whether the payload names the run in a `run_id` field, or the event is
/// attributed to it as stored events are by their `run_id` column.
fn attributed_to_run(payload: &Value, run_id: &str) -> bool {
    names_run(payload, run_id)
        || serde_json::from_value::<event_bus::EventKind>(payload.clone())
            .is_ok_and(|kind| kind.primary_run_id() == Some(run_id))
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
        ValueRef::Blob(bytes) => match JSON_COLUMNS
            .contains(&name)
            .then(|| crate::codec::decode(value))
        {
            Some(Ok(text)) => {
                serde_json::from_str(&text).unwrap_or_else(|_| Value::String(text.into_owned()))
            }
            Some(Err(error)) => {
                json!({ "blob_bytes": bytes.len(), "undecodable": error.to_string() })
            }
            None => json!({ "blob_bytes": bytes.len() }),
        },
    }
}
