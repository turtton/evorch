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

mod bodies;
mod lanes;
mod open;
mod storage_summary;
pub use bodies::{
    BLOB_PREVIEW_BYTES, BlobOptions, BodyCheckScope, DEFAULT_REFERENCES_LISTED,
    MAX_REFERENCES_LISTED, MIN_BLOB_PREFIX_DIGITS,
};
pub use lanes::{LANE_DIAGNOSTICS_LIST_MAX, LANE_DIAGNOSTICS_UNCOLLECTED_EVALUATED_MAX};
pub use open::InspectedDatabase;
pub use storage_summary::{DEFAULT_TOP_THREADS, StorageSummaryOptions};

/// Wording of the refusal to read a schema older than the current one, which
/// [`StorageError::class`] recognises.
pub(crate) const OLDER_SCHEMA: &str = "is older than";

/// Columns holding JSON text that are expanded in inspection output.
const JSON_COLUMNS: [&str; 5] = [
    "payload",
    "config_json",
    "checkpoints_json",
    "recent_run_ids",
    "body",
];

/// How much of a run's conversation `inspect run` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunDetail {
    /// Counts only: no message text and no checkpoint list.
    Summary,
    /// Every message and the checkpoint list, bodies resolved.
    Full,
    /// Messages `from..to` only (ordinals of the stored array, System messages included,
    /// not the provenance ordinals), plus the checkpoint list. Messages outside the range
    /// are not read.
    Messages(std::ops::Range<usize>),
}

/// A stored event with its payload kept as raw JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct InspectEvent {
    pub id: i64,
    pub session_id: Option<String>,
    pub wall_clock_ns: i64,
    pub kind: String,
    /// The event's JSON. A tokenized event shows its bodies resolved
    /// ([`InspectBodies::Resolved`]) or its tokens ([`InspectBodies::Refs`]).
    pub payload: Value,
    /// `events.body_format`: 0 inline, 1 tokenized.
    pub body_format: i64,
    /// Why a body could not be resolved (the tokens stay in `payload`); never body text.
    pub body_error: Option<String>,
}

/// How `inspect events` shows the bodies of a tokenized event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InspectBodies {
    /// Bodies are read and put where their tokens are (the default).
    #[default]
    Resolved,
    /// Tokens (hash and length) stay as they are; no blob data is read to show them.
    Refs,
}

/// Options of [`Database::inspect_events_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct InspectEventsOptions<'a> {
    pub bodies: InspectBodies,
    /// Keep only events with a string value (a resolved body included) that contains this
    /// text. It is matched against the resolved bodies whatever `bodies` shows.
    pub contains: Option<&'a str>,
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
        Self::checked(conn)
    }

    /// Opens an existing database as an immutable snapshot: no locks, no journal and no
    /// `-wal`/`-shm` files are used or created. Only correct for a file nothing writes, such
    /// as a finished evaluation run's database; the caller must make sure no `-wal` or
    /// `-journal` file holds unapplied changes. `path` may be `/proc/self/fd/N` to reach the
    /// file an already verified descriptor refers to; SQLite expands that name to an
    /// ordinary path before opening it, so this does not protect against someone replacing
    /// the file concurrently and on purpose (the directory is assumed to be the caller's).
    ///
    /// # Errors
    /// Fails like [`Database::open_read_only`].
    pub fn open_read_only_immutable(path: &Path) -> Result<Self, StorageError> {
        let text = path
            .to_str()
            .ok_or(StorageError::OutOfRange("database path is not UTF-8"))?;
        let mut uri = String::from("file:");
        for character in text.chars() {
            match character {
                '%' => uri.push_str("%25"),
                '?' => uri.push_str("%3f"),
                '#' => uri.push_str("%23"),
                other => uri.push(other),
            }
        }
        uri.push_str("?immutable=1");
        let conn = Connection::open_with_flags(
            uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )?;
        Self::checked(conn)
    }

    fn checked(conn: Connection) -> Result<Self, StorageError> {
        conn.busy_timeout(std::time::Duration::from_millis(5_000))?;
        let found = conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))?;
        let supported = u32::try_from(crate::migrations::MIGRATIONS.len())
            .map_err(|_| StorageError::OutOfRange("migration count"))?;
        if found > supported {
            return Err(StorageError::SchemaTooNew { found, supported });
        }
        // An old conversation format (or a half-converted file) is refused, not read.
        crate::migrations::require_current_format(&conn)?;
        if found < supported {
            return Err(StorageError::Serialization(format!(
                "schema version {found} {OLDER_SCHEMA} {supported}; stop every evorch process and run the conversion command before inspecting it"
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
        self.inspect_events_with(scope, limit, InspectEventsOptions::default())
    }

    /// [`Self::inspect_events`] with the way bodies are shown and a text filter.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn inspect_events_with(
        &self,
        scope: EventScope<'_>,
        limit: usize,
        options: InspectEventsOptions<'_>,
    ) -> Result<(Vec<InspectEvent>, bool), StorageError> {
        let limit = limit.clamp(1, 10_000);
        let mut events = Vec::new();
        let mut truncated = false;
        // Events of a deleted family stay in the file until reclaimed; they are not
        // shown meanwhile, by run column or by what the payload says it belongs to.
        let tombstones = crate::thread_retention::TombstoneSet::load(&self.conn)?;
        match scope {
            EventScope::Run(run_id) if tombstones.contains_run(run_id) => {}
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
                    "SELECT id, session_id, wall_clock_ns, kind, payload, run_id, body_format \
                     FROM events WHERE id = ?1",
                )?;
                for candidate in normalize(candidates).iter().rev() {
                    let mut rows = statement.query([candidate.id])?;
                    let Some(row) = rows.next()? else { continue };
                    let (event, resolved) = event_row(&transaction, row, options)?;
                    if hidden(&tombstones, &resolved, row.get::<_, Option<String>>(5)?) {
                        continue;
                    }
                    if candidate.confirm && !attributed_to_run(&resolved, run_id) {
                        continue;
                    }
                    if !contains_text(&resolved, options.contains) {
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
                // Hidden events are dropped after the statement, so it is paged by id
                // until `limit` visible events and one more are found or the window ends.
                let page = i64::try_from(limit + 1).unwrap_or(i64::MAX);
                let mut before = i64::MAX;
                let mut statement = self.conn.prepare(
                    "SELECT id, session_id, wall_clock_ns, kind, payload, run_id, body_format
                     FROM events WHERE wall_clock_ns BETWEEN ?1 AND ?2 AND id < ?4
                     AND (run_id IS NULL OR run_id NOT IN (SELECT run_id FROM run_tombstones))
                     ORDER BY id DESC LIMIT ?3",
                )?;
                'pages: loop {
                    let mut rows =
                        statement.query(rusqlite::params![from_ns, to_ns, page, before])?;
                    let mut read = 0_i64;
                    while let Some(row) = rows.next()? {
                        read += 1;
                        let (event, resolved) = event_row(&self.conn, row, options)?;
                        before = event.id;
                        if hidden(&tombstones, &resolved, row.get::<_, Option<String>>(5)?) {
                            continue;
                        }
                        if !contains_text(&resolved, options.contains) {
                            continue;
                        }
                        if events.len() == limit {
                            truncated = true;
                            break 'pages;
                        }
                        events.push(event);
                    }
                    if read < page {
                        break;
                    }
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
        self.inspect_run_detail(
            run_id,
            if full {
                RunDetail::Full
            } else {
                RunDetail::Summary
            },
        )
    }

    /// [`Database::inspect_run`] with a choice of how much of the conversation to read.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails, if a body of the conversation cannot be
    /// resolved (never replaced by a placeholder), or if the range is outside the run.
    pub fn inspect_run_detail(
        &self,
        run_id: &str,
        detail: RunDetail,
    ) -> Result<Value, StorageError> {
        // A confirmed deletion hides the run's rows until they are reclaimed; the
        // usage ledger and learning records are kept on purpose and still show.
        let context = crate::repo::run_messages::read_snapshot(&self.conn, || {
            self.inspect_context(run_id, &detail)
        })?;
        let children: Vec<Value> = self
            .json_rows(
                "SELECT run_id, role, name, terminal_phase, updated_at_ns FROM run_contexts
                 WHERE parent_run_id = ?1 AND run_id NOT IN (SELECT run_id FROM run_tombstones)
                 ORDER BY updated_at_ns",
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
            "ledger": self.json_rows(
                "SELECT * FROM run_ledger WHERE run_id = ?1 \
                 AND run_id NOT IN (SELECT run_id FROM run_tombstones) ORDER BY seq",
                &[run_id],
            )?,
            "usage_requests": self.json_rows("SELECT * FROM usage_requests WHERE run_id = ?1 ORDER BY at_ns", &[run_id])?,
            "user_questions": self.json_rows(
                "SELECT * FROM user_questions WHERE (run_id = ?1
                 OR id IN (SELECT question_id FROM user_question_links WHERE run_id = ?1))
                 AND run_id NOT IN (SELECT run_id FROM run_tombstones)
                 AND root_run_id NOT IN (SELECT run_id FROM run_tombstones)
                 AND ?1 NOT IN (SELECT run_id FROM run_tombstones)",
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

    /// The run header as inspection output: configuration and checkpoint list with their
    /// bodies resolved, the stored projections, and the messages `detail` asks for.
    fn inspect_context(
        &self,
        run_id: &str,
        detail: &RunDetail,
    ) -> Result<Option<Value>, StorageError> {
        let mut context = self
            .json_rows(
                "SELECT run_id, role, name, parent_run_id, config_json, checkpoints_json, \
                 terminal_phase, restorable, updated_at_ns, snapshot_revision, host_inputs_json, \
                 body_format, header_rev, message_count, messages_len, checkpoints_len, \
                 config_len, non_restorable_reason FROM run_contexts WHERE run_id = ?1 \
                 AND run_id NOT IN (SELECT run_id FROM run_tombstones)",
                &[run_id],
            )?
            .into_iter()
            .next();
        let Some(Value::Object(object)) = context.as_mut() else {
            return Ok(None);
        };
        // `json_rows` parsed the stored (tokenized) text. The configuration is shown
        // resolved in every view; the checkpoint list only when message text is asked for,
        // because its bodies are conversation text.
        let stored_config = self.conn.query_row(
            "SELECT config_json FROM run_contexts WHERE run_id = ?1",
            [run_id],
            |row| row.get::<_, String>(0),
        )?;
        object.insert(
            "config_json".to_owned(),
            crate::repo::run_messages::resolved_document(&self.conn, run_id, &stored_config)?,
        );
        match detail {
            RunDetail::Summary => {
                // The list is an array whose elements are objects: tokens replace only
                // string values, so the stored form has the element count without
                // resolving one body.
                if let Some(list) = object.remove("checkpoints_json") {
                    let count = list.as_array().map(Vec::len).ok_or_else(|| {
                        StorageError::Serialization(
                            "the stored checkpoint list is not an array".into(),
                        )
                    })?;
                    object.insert("checkpoints_count".into(), json!(count));
                }
                let count = object.get("message_count").cloned().unwrap_or(json!(0));
                object.insert("messages_count".into(), count);
                object.insert("bodies".into(), self.run_body_stats(run_id)?);
            }
            RunDetail::Full | RunDetail::Messages(_) => {
                let stored_checkpoints = self.conn.query_row(
                    "SELECT checkpoints_json FROM run_contexts WHERE run_id = ?1",
                    [run_id],
                    |row| row.get::<_, String>(0),
                )?;
                object.insert(
                    "checkpoints_json".to_owned(),
                    crate::repo::run_messages::resolved_document(
                        &self.conn,
                        run_id,
                        &stored_checkpoints,
                    )?,
                );
                let range = match detail {
                    RunDetail::Messages(range) => Some(range.clone()),
                    _ => None,
                };
                let (_, messages) =
                    crate::repo::run_messages::message_range(&self.conn, run_id, range.clone())?
                        .ok_or_else(|| {
                            StorageError::Serialization("run context vanished".into())
                        })?;
                let parsed = messages
                    .iter()
                    .map(|text| serde_json::from_str::<Value>(text))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| StorageError::Serialization(error.to_string()))?;
                if let Some(range) = range {
                    object.insert(
                        "messages_range".into(),
                        json!({ "from": range.start, "to": range.end }),
                    );
                }
                object.insert("messages_json".into(), Value::Array(parsed));
            }
        }
        Ok(context)
    }

    /// What the run keeps in content blobs, from the reference tables and the blob rows'
    /// metadata only (`byte_len`, `length(data)`: no body is read or decompressed).
    /// `messages` and `header` count references; a blob two of them name is counted in
    /// each, and `distinct_blobs` counts it once.
    fn run_body_stats(&self, run_id: &str) -> Result<Value, StorageError> {
        let stats = |sql: &str| -> Result<Value, StorageError> {
            self.conn
                .query_row(sql, [run_id], |row| {
                    Ok(json!({
                        "references": row.get::<_, i64>(0)?,
                        "distinct_blobs": row.get::<_, i64>(1)?,
                        "plain_bytes": row.get::<_, i64>(2)?,
                        "stored_bytes": row.get::<_, i64>(3)?,
                    }))
                })
                .map_err(StorageError::from)
        };
        let skeletons: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(OCTET_LENGTH(skeleton)), 0) FROM run_messages WHERE run_id = ?1",
            [run_id],
            |row| row.get(0),
        )?;
        Ok(json!({
            "message_skeleton_bytes": skeletons,
            "message_blobs": stats(
                "SELECT COUNT(*), COUNT(DISTINCT r.blob_id), COALESCE(SUM(b.byte_len), 0), \
                 COALESCE(SUM(OCTET_LENGTH(b.data)), 0) FROM run_messages m \
                 JOIN content_blob_refs_message r ON r.message_id = m.id \
                 JOIN content_blobs b ON b.id = r.blob_id WHERE m.run_id = ?1",
            )?,
            "header_blobs": stats(
                "SELECT COUNT(*), COUNT(DISTINCT r.blob_id), COALESCE(SUM(b.byte_len), 0), \
                 COALESCE(SUM(OCTET_LENGTH(b.data)), 0) FROM content_blob_refs_header r \
                 JOIN content_blobs b ON b.id = r.blob_id WHERE r.run_id = ?1",
            )?,
        }))
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
    let mut value = json!({
        "id": event.id,
        "session_id": event.session_id,
        "wall_clock_ns": event.wall_clock_ns,
        "kind": event.kind,
        "payload": event.payload,
    });
    if event.body_format != 0 {
        value["body_format"] = json!(event.body_format);
    }
    if let Some(error) = &event.body_error {
        value["body_error"] = json!(error);
    }
    value
}

/// Whether a stored diagnostic is a sandbox escalation review manifest.
fn is_review_manifest(payload: &Value) -> bool {
    let diagnostic = &payload["payload"];
    diagnostic["source"] == "sandbox" && diagnostic["code"] == "escalation_review_manifest"
}

/// An event row, and its JSON for attribution and the text filter. The returned event
/// shows `bodies`. Bodies are read only when they are shown or searched: attribution looks
/// at `run_id` fields and the event variant, which a token never replaces, so `refs` without
/// a filter reads no blob. A body that cannot be resolved does not fail the inspection: its
/// tokens stay and `body_error` says why.
fn event_row(
    conn: &Connection,
    row: &rusqlite::Row<'_>,
    options: InspectEventsOptions<'_>,
) -> Result<(InspectEvent, Value), StorageError> {
    let id: i64 = row.get(0)?;
    let body_format: i64 = row.get(6)?;
    let mut payload = column_json("payload", row.get_ref(4)?);
    let mut resolved = payload.clone();
    let mut body_error = None;
    let needs_bodies = options.bodies == InspectBodies::Resolved || options.contains.is_some();
    if body_format == 1 && needs_bodies {
        let label = format!("event {id}");
        match crate::body_store::resolve_container(conn, &label, &mut resolved) {
            Ok(()) => {
                if options.bodies == InspectBodies::Resolved {
                    payload = resolved.clone();
                }
            }
            Err(error @ StorageError::BodyUnresolved { .. }) => {
                body_error = Some(error.to_string());
                resolved = payload.clone();
            }
            Err(other) => return Err(other),
        }
    }
    Ok((
        InspectEvent {
            id,
            session_id: row.get(1)?,
            wall_clock_ns: row.get(2)?,
            kind: row.get(3)?,
            payload,
            body_format,
            body_error,
        },
        resolved,
    ))
}

/// Whether some string value of `value` contains `needle` (`None` keeps everything).
fn contains_text(value: &Value, needle: Option<&str>) -> bool {
    let Some(needle) = needle else { return true };
    match value {
        Value::String(text) => text.contains(needle),
        Value::Array(items) => items.iter().any(|item| contains_text(item, Some(needle))),
        Value::Object(map) => map.values().any(|item| contains_text(item, Some(needle))),
        _ => false,
    }
}

/// Whether the event belongs to a deleted run or thread.
fn hidden(
    tombstones: &crate::thread_retention::TombstoneSet,
    payload: &Value,
    run_id: Option<String>,
) -> bool {
    run_id.is_some_and(|run| tombstones.contains_run(&run))
        || serde_json::from_value::<event_bus::EventKind>(payload.clone())
            .is_ok_and(|kind| tombstones.rejects(&kind).is_some())
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
