//! The v39 source of the conversion: its shape, the old per-run row, and the read-only
//! analysis that `preview` reports and `--apply` partly repeats (the size limit).

use std::collections::HashSet;
use std::io::Read;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;

use crate::StorageError;
use crate::body_ref::{BodyToken, tokenize_in_place};
use crate::migrations::{MESSAGE_ROWS_VERSION, MIGRATIONS, require_current_format};
use crate::repo::run_messages::to_json;

use super::super::invalid;

/// Columns of a v39 `run_contexts`, in table order. Anything else is not a database this
/// command knows how to convert.
const SOURCE_COLUMNS: [&str; 14] = [
    "run_id",
    "role",
    "name",
    "parent_run_id",
    "config_json",
    "messages_json",
    "checkpoints_json",
    "terminal_phase",
    "restorable",
    "updated_at_ns",
    "host_inputs_json",
    "snapshot_revision",
    "messages_len",
    "checkpoints_len",
];

/// Tables a v39 database has that the conversion relies on.
const REQUIRED_TABLES: [&str; 4] = [
    "run_tombstones",
    "content_blobs",
    "content_blob_gc_queue",
    "reclaim_jobs",
];

/// Tables migration v40 creates: none of them may exist before the conversion starts.
const TARGET_TABLES: [&str; 3] = [
    "run_messages",
    "content_blob_refs_message",
    "content_blob_refs_header",
];

/// The most plain bytes (messages, checkpoints and configuration) of one run the
/// conversion holds in memory. A run above it is reported by `preview` and refused by
/// `--apply`; the limit is not raised to make a conversion pass.
pub(crate) const MAX_RUN_BYTES: u64 = 1_000_000_000;

/// The limit in force: [`MAX_RUN_BYTES`], or a smaller one a test sets so that the
/// boundary can be exercised with small data.
pub(super) fn run_limit() -> u64 {
    #[cfg(test)]
    if let Some(limit) = limit_override::get() {
        return limit;
    }
    MAX_RUN_BYTES
}

#[cfg(test)]
pub(super) mod limit_override {
    use std::cell::Cell;

    thread_local! {
        static LIMIT: Cell<Option<u64>> = const { Cell::new(None) };
    }

    pub(in crate::maintenance::convert_bodies) fn get() -> Option<u64> {
        LIMIT.with(Cell::get)
    }

    pub(in crate::maintenance::convert_bodies) fn set(limit: Option<u64>) {
        LIMIT.with(|cell| cell.set(limit));
    }
}

/// Problems `preview` lists; the count goes on.
const MAX_LISTED_PROBLEMS: usize = 20;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Shape {
    /// A regular v39 database: the conversion applies.
    Source,
    /// Already in the current format: nothing to do.
    Converted,
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool, StorageError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [name],
        |row| row.get(0),
    )?)
}

pub(super) fn restore_hint() -> &'static str {
    "restore the verified backup taken before the conversion and run it again from the start"
}

/// Decides what the database is, reading nothing but the schema.
pub(super) fn classify(conn: &Connection) -> Result<Shape, StorageError> {
    let version: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let current =
        u32::try_from(MIGRATIONS.len()).map_err(|_| StorageError::OutOfRange("migration count"))?;
    if version > current {
        return Err(StorageError::SchemaTooNew {
            found: version,
            supported: current,
        });
    }
    if version == current {
        require_current_format(conn)?;
        return Ok(Shape::Converted);
    }
    if version + 1 != MESSAGE_ROWS_VERSION {
        return Err(invalid(format!(
            "convert-bodies converts a version {} database; this one is version {version}",
            MESSAGE_ROWS_VERSION - 1
        )));
    }
    let columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('run_contexts')")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let mut leftovers = columns
        .iter()
        .any(|column| column == "body_format" || column == "header_rev");
    for table in TARGET_TABLES {
        leftovers |= table_exists(conn, table)?;
    }
    if leftovers {
        return Err(invalid(format!(
            "the database has the tables or columns of a conversion that did not finish; {}",
            restore_hint()
        )));
    }
    if columns != SOURCE_COLUMNS {
        return Err(invalid(
            "run_contexts does not have the columns of a version 39 database",
        ));
    }
    for table in REQUIRED_TABLES {
        if !table_exists(conn, table)? {
            return Err(invalid(format!(
                "the version 39 table {table} is missing; the database is not a regular one"
            )));
        }
    }
    Ok(Shape::Source)
}

/// A run as the old format stores it.
#[derive(Debug)]
pub(super) struct OldRun {
    pub run_id: String,
    pub restorable: bool,
    pub config: String,
    pub messages: String,
    pub checkpoints: String,
}

/// A run read from the old columns, or the reason it cannot be (never any of its text).
pub(super) enum OldRead {
    Run(OldRun),
    Fault(&'static str),
}

/// The text of a stored value, never holding more than `remaining` bytes: a TEXT value is
/// measured, a zstd frame is checked against its declared size and decompressed through a
/// reader that stops one byte past the limit (a frame may lie about its size).
pub(super) fn decode_bounded(value: ValueRef<'_>, remaining: u64) -> Result<String, &'static str> {
    const TOO_BIG: &str = "a document is larger than the in-memory limit";
    let bytes = match value {
        ValueRef::Text(bytes) => {
            if bytes.len() as u64 > remaining {
                return Err(TOO_BIG);
            }
            bytes.to_vec()
        }
        ValueRef::Blob(bytes) => {
            if let Ok(Some(declared)) = zstd::zstd_safe::get_frame_content_size(bytes)
                && declared > remaining
            {
                return Err(TOO_BIG);
            }
            let decoder = zstd::stream::read::Decoder::new(bytes)
                .map_err(|_| "a stored document is not a zstd frame")?;
            let mut plain = Vec::new();
            decoder
                .take(remaining.saturating_add(1))
                .read_to_end(&mut plain)
                .map_err(|_| "a stored document does not decompress")?;
            if plain.len() as u64 > remaining {
                return Err(TOO_BIG);
            }
            plain
        }
        _ => return Err("a stored document is neither TEXT nor a BLOB"),
    };
    String::from_utf8(bytes).map_err(|_| "a stored document is not UTF-8")
}

/// Reads one run's documents. Their plain size together may not pass [`MAX_RUN_BYTES`]:
/// the limit is applied while decompressing, not after.
pub(super) fn read_old_run(conn: &Connection, run_id: &str) -> Result<OldRead, StorageError> {
    let mut statement = conn.prepare_cached(
        "SELECT run_id, restorable, config_json, messages_json, checkpoints_json \
         FROM run_contexts WHERE run_id = ?1",
    )?;
    let mut rows = statement.query([run_id])?;
    let Some(row) = rows.next()? else {
        return Err(invalid(format!(
            "run {run_id} disappeared during the conversion"
        )));
    };
    let mut remaining = run_limit();
    let mut document = |index: usize| -> Result<Result<String, &'static str>, StorageError> {
        let decoded = decode_bounded(row.get_ref(index)?, remaining);
        if let Ok(text) = &decoded {
            remaining -= text.len() as u64;
        }
        Ok(decoded)
    };
    let config = match document(2)? {
        Ok(text) => text,
        Err(reason) => return Ok(OldRead::Fault(reason)),
    };
    let messages = match document(3)? {
        Ok(text) => text,
        Err(reason) => return Ok(OldRead::Fault(reason)),
    };
    let checkpoints = match document(4)? {
        Ok(text) => text,
        Err(reason) => return Ok(OldRead::Fault(reason)),
    };
    Ok(OldRead::Run(OldRun {
        run_id: row.get(0)?,
        restorable: row.get(1)?,
        config,
        messages,
        checkpoints,
    }))
}

/// The next run after `after` (`None` starts), in `run_id` order. With `skip_deleted`
/// the runs a retention deleted (tombstoned, rows not yet reclaimed) are not visited;
/// with `within_limit` neither are the runs whose recorded size is above [`MAX_RUN_BYTES`].
pub(super) fn next_run(
    conn: &Connection,
    after: Option<&str>,
    skip_deleted: bool,
    within_limit: bool,
) -> Result<Option<String>, StorageError> {
    let mut filter = String::new();
    if skip_deleted {
        filter.push_str(" AND run_id NOT IN (SELECT run_id FROM run_tombstones)");
    }
    if within_limit {
        filter.push_str(&format!(" AND {RUN_BYTES_SQL} <= {}", run_limit()));
    }
    Ok(conn
        .prepare_cached(&format!(
            "SELECT run_id FROM run_contexts WHERE (?1 IS NULL OR run_id > ?1){filter} \
             ORDER BY run_id LIMIT 1"
        ))?
        .query_row(params![after], |row| row.get(0))
        .optional()?)
}

/// The plain bytes of a run as its columns record them (no decoding).
const RUN_BYTES_SQL: &str = "messages_len + checkpoints_len + length(CAST(config_json AS BLOB))";

/// Runs whose plain size is above [`MAX_RUN_BYTES`], from the length columns alone.
pub(super) fn oversized_runs(conn: &Connection) -> Result<Vec<String>, StorageError> {
    let mut statement = conn.prepare(&format!(
        "SELECT run_id FROM run_contexts WHERE {RUN_BYTES_SQL} > ?1 \
         AND run_id NOT IN (SELECT run_id FROM run_tombstones) ORDER BY run_id LIMIT 20"
    ))?;
    let ids = statement.query_map([run_limit()], |row| row.get(0))?;
    Ok(ids.collect::<Result<_, _>>()?)
}

/// A run's documents in the form the conversion stores them.
pub(super) struct Documents {
    pub messages: Value,
    /// Each message's serialization; their join is `messages_json`.
    pub plain: Vec<String>,
    pub messages_json: String,
    pub checkpoints: Value,
    pub checkpoints_json: String,
    /// The old bytes are not what the serializer writes (an old key order), and the run
    /// is not restorable, so the stored form is the serializer's, not the old bytes.
    pub canonicalized: bool,
}

/// Parses a run's documents and decides what is stored.
///
/// The bytes the old column held must be exactly what the serializer writes, with one
/// exception: a run that cannot be restored (`restorable = 0`) is never sent to a
/// provider again, so its documents may be stored in the serializer's form (keys in
/// order) when they are the same JSON value. A restorable run whose bytes differ stops
/// the conversion. The error is the reason, without any of the text.
pub(super) fn documents(run: &OldRun) -> Result<Documents, &'static str> {
    let messages =
        serde_json::from_str::<Value>(&run.messages).map_err(|_| "messages are not valid JSON")?;
    let Some(array) = messages.as_array() else {
        return Err("messages are not a JSON array");
    };
    let mut plain = Vec::with_capacity(array.len());
    for message in array {
        plain.push(to_json(message).map_err(|_| "a message does not serialize")?);
    }
    let messages_json = format!("[{}]", plain.join(","));
    let checkpoints = serde_json::from_str::<Value>(&run.checkpoints)
        .map_err(|_| "checkpoints are not valid JSON")?;
    let checkpoints_json = to_json(&checkpoints).map_err(|_| "checkpoints do not serialize")?;
    if serde_json::from_str::<Value>(&run.config).is_err() {
        return Err("configuration is not valid JSON");
    }
    let messages_differ = messages_json != run.messages;
    let checkpoints_differ = checkpoints_json != run.checkpoints;
    if run.restorable {
        if messages_differ {
            return Err("a restorable run's messages are not the serialization of their parse");
        }
        if checkpoints_differ {
            return Err("a restorable run's checkpoints are not the serialization of their parse");
        }
    } else {
        let same_value = |text: &str, value: &Value| {
            serde_json::from_str::<Value>(text).is_ok_and(|parsed| &parsed == value)
        };
        if messages_differ && !same_value(&messages_json, &messages) {
            return Err("the serialized messages are another JSON value");
        }
        if checkpoints_differ && !same_value(&checkpoints_json, &checkpoints) {
            return Err("the serialized checkpoints are another JSON value");
        }
    }
    Ok(Documents {
        messages,
        plain,
        messages_json,
        checkpoints,
        checkpoints_json,
        canonicalized: messages_differ || checkpoints_differ,
    })
}

#[derive(Debug, Serialize)]
pub struct Problem {
    pub run_id: String,
    pub reason: &'static str,
}

/// What `preview` measured. Fields named `estimated_*` are predictions from a pass that
/// stores nothing; the others are counts or sums read from the database.
#[derive(Debug, Serialize, Default)]
pub struct Analysis {
    /// Runs the conversion will convert.
    pub convertible_runs: u64,
    /// Runs a retention deleted whose rows are still there; they are reclaimed first.
    pub deleted_runs_to_reclaim: u64,
    pub reclaim_jobs_pending: u64,
    pub reclaim_oversized_registered: u64,
    /// Stored bytes of the old messages and checkpoints of the convertible runs.
    pub old_payload_stored_bytes: u64,
    /// Their plain bytes.
    pub old_payload_plain_bytes: u64,
    pub largest_run_plain_bytes: u64,
    /// Runs above the in-memory limit (the first few ids): `--apply` refuses while there
    /// is one, and neither this pass nor the conversion reads them.
    pub oversized_runs: Vec<String>,
    pub oversized_run_count: u64,
    /// Filled by the full pass only.
    pub messages: u64,
    pub estimated_blob_candidates: u64,
    /// Bytes of every candidate string, shared ones counted each time: the upper bound
    /// of what the blobs can take, and the figure the growth estimate uses.
    pub estimated_candidate_plain_bytes: u64,
    /// Distinct bodies seen; a lower bound once `distinct_tracking_capped` is set.
    pub estimated_distinct_blobs: u64,
    pub estimated_blob_plain_bytes: u64,
    /// The set of distinct bodies reached its limit and stopped growing.
    pub distinct_tracking_capped: bool,
    /// Runs whose documents are not JSON, or do not survive the byte round trip while the
    /// run is restorable: `--apply` stops at the first.
    pub unconvertible_runs: u64,
    /// Runs that cannot be restored and whose old bytes are not the serializer's form
    /// (an old key order): they are stored in the serializer's form, as the same JSON
    /// value. Their conversation index no longer matches their snapshot hash, so it is
    /// built again.
    pub canonicalizable_runs: u64,
    /// Upper bound of how much the database grows before the VACUUM (new rows, indexes
    /// and blobs, every candidate stored without compression).
    pub estimated_growth_bytes: u64,
    /// Reasons a reclaim of deleted runs cannot be completed (a cursor this build does
    /// not read, a deleted run with no job). `--apply` refuses while there is one.
    pub reclaim_blockers: Vec<String>,
    pub problems: Vec<Problem>,
}

fn scalar(conn: &Connection, sql: &str) -> Result<u64, StorageError> {
    Ok(conn
        .query_row(sql, [], |row| row.get::<_, Option<u64>>(0))?
        .unwrap_or(0))
}

/// The counts that need no decoding.
pub(super) fn measure(conn: &Connection) -> Result<Analysis, StorageError> {
    let not_deleted = "run_id NOT IN (SELECT run_id FROM run_tombstones)";
    Ok(Analysis {
        convertible_runs: scalar(
            conn,
            &format!("SELECT COUNT(*) FROM run_contexts WHERE {not_deleted}"),
        )?,
        deleted_runs_to_reclaim: scalar(
            conn,
            "SELECT COUNT(*) FROM run_contexts \
             WHERE run_id IN (SELECT run_id FROM run_tombstones)",
        )?,
        reclaim_jobs_pending: scalar(
            conn,
            "SELECT COUNT(*) FROM reclaim_jobs WHERE state = 'db_pending'",
        )?,
        reclaim_oversized_registered: scalar(conn, "SELECT COUNT(*) FROM reclaim_oversized")?,
        old_payload_stored_bytes: scalar(
            conn,
            &format!(
                "SELECT SUM(OCTET_LENGTH(messages_json) + OCTET_LENGTH(checkpoints_json)) \
                 FROM run_contexts WHERE {not_deleted}"
            ),
        )?,
        old_payload_plain_bytes: scalar(
            conn,
            &format!(
                "SELECT SUM(messages_len + checkpoints_len) FROM run_contexts WHERE {not_deleted}"
            ),
        )?,
        largest_run_plain_bytes: scalar(
            conn,
            &format!("SELECT MAX({RUN_BYTES_SQL}) FROM run_contexts WHERE {not_deleted}"),
        )?,
        oversized_runs: oversized_runs(conn)?,
        oversized_run_count: scalar(
            conn,
            &format!(
                "SELECT COUNT(*) FROM run_contexts WHERE {RUN_BYTES_SQL} > {} \
                 AND {not_deleted}",
                run_limit()
            ),
        )?,
        reclaim_blockers: super::retention::blockers(conn)?,
        ..Analysis::default()
    })
}

/// Distinct bodies [`analyze`] remembers (40 bytes each). Past it the count of distinct
/// bodies is a lower bound; the growth estimate does not depend on it.
const MAX_TRACKED_BODIES: usize = 2_000_000;

/// The full read-only pass of `preview` and of `--apply`'s check before anything changes:
/// every convertible run within the size limit is decoded (bounded), round-tripped and
/// tokenized without storing anything. One run is in memory at a time; the only state
/// kept across runs is the hash and length of each distinct body, up to a limit.
pub(super) fn analyze(conn: &Connection) -> Result<Analysis, StorageError> {
    analyze_with_cap(conn, MAX_TRACKED_BODIES)
}

pub(super) fn analyze_with_cap(conn: &Connection, cap: usize) -> Result<Analysis, StorageError> {
    let mut analysis = measure(conn)?;
    let mut bodies: HashSet<BodyToken> = HashSet::new();
    let mut cursor: Option<String> = None;
    while let Some(run_id) = next_run(conn, cursor.as_deref(), true, true)? {
        let problem = match read_old_run(conn, &run_id)? {
            OldRead::Fault(reason) => Some(reason),
            OldRead::Run(run) => match documents(&run) {
                Err(reason) => Some(reason),
                Ok(documents) => {
                    analysis.canonicalizable_runs += u64::from(documents.canonicalized);
                    count_bodies(&run, &mut analysis, &mut bodies, cap)?;
                    None
                }
            },
        };
        if let Some(reason) = problem {
            analysis.unconvertible_runs += 1;
            if analysis.problems.len() < MAX_LISTED_PROBLEMS {
                analysis.problems.push(Problem {
                    run_id: run_id.clone(),
                    reason,
                });
            }
        }
        cursor = Some(run_id);
    }
    analysis.estimated_distinct_blobs = bodies.len() as u64;
    analysis.estimated_blob_plain_bytes = bodies.iter().map(|token| token.byte_len).sum();
    analysis.estimated_growth_bytes = analysis
        .estimated_candidate_plain_bytes
        .saturating_add(analysis.messages.saturating_mul(GROWTH_PER_MESSAGE))
        .saturating_add(
            analysis
                .estimated_blob_candidates
                .saturating_mul(GROWTH_PER_BLOB),
        )
        .saturating_add(analysis.convertible_runs.saturating_mul(GROWTH_PER_RUN));
    Ok(analysis)
}

/// Bytes a message row (columns, skeleton overhead and its two indexes), a blob or
/// reference row, and a header add beyond the strings: deliberately generous.
const GROWTH_PER_MESSAGE: u64 = 384;
const GROWTH_PER_BLOB: u64 = 160;
const GROWTH_PER_RUN: u64 = 2_048;

fn count_bodies(
    run: &OldRun,
    analysis: &mut Analysis,
    bodies: &mut HashSet<BodyToken>,
    cap: usize,
) -> Result<(), StorageError> {
    let serde_error = |error: serde_json::Error| StorageError::Serialization(error.to_string());
    let mut candidates = 0_u64;
    let mut candidate_bytes = 0_u64;
    let mut capped = analysis.distinct_tracking_capped;
    let mut visit = |value: &mut Value| {
        tokenize_in_place(value, &mut |text| {
            let token = BodyToken::of(text);
            candidates += 1;
            candidate_bytes += token.byte_len;
            if bodies.len() < cap {
                bodies.insert(token);
            } else if !bodies.contains(&token) {
                capped = true;
            }
            Ok(token)
        })
    };
    let mut messages: Value = serde_json::from_str(&run.messages).map_err(serde_error)?;
    let mut count = 0_u64;
    if let Some(array) = messages.as_array_mut() {
        for message in array {
            visit(message)?;
            count += 1;
        }
    }
    let mut checkpoints: Value = serde_json::from_str(&run.checkpoints).map_err(serde_error)?;
    visit(&mut checkpoints)?;
    let mut config: Value = serde_json::from_str(&run.config).map_err(serde_error)?;
    visit(&mut config)?;
    analysis.messages += count;
    analysis.estimated_blob_candidates += candidates;
    analysis.estimated_candidate_plain_bytes += candidate_bytes;
    analysis.distinct_tracking_capped = capped;
    Ok(())
}
