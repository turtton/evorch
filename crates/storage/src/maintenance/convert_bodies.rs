//! `evorch maintenance convert-bodies`: the one-time offline conversion of a database of
//! version 33 to 39 (a run's whole conversation in `run_contexts.messages_json`; the
//! migrations up to 39 are applied first, after the backup) to the current
//! format (`run_messages` rows and `content_blobs`, and a provenance map as
//! `run_message_provenance` rows), including the removal of the two old columns. A regular
//! version 40 database (messages already rows, maps still one column) is accepted by this
//! command alone: its runs are not converted again, only its provenance maps.
//!
//! # Contract
//!
//! * Every evorch process is stopped. The command holds the maintenance lease and
//!   SQLite's exclusive locking mode from the first write to the last.
//! * A non-empty WAL is always refused, at preview and at apply. A conversion that was
//!   interrupted is recovered one way only: restore the verified backup taken before it
//!   started and run the command again from the beginning. There is no resume.
//! * The verified backup is made before the first change. Rows of runs that a confirmed
//!   retention already deleted are reclaimed next (they are not converted), and the
//!   reclaim must finish or the command stops.
//! * Each run is converted in its own transaction, read back through the normal reader
//!   and compared byte for byte with what the old column held before it commits. Then
//!   each provenance map is converted in its own transaction and compared, as the typed
//!   map, with the old one (`stage_snapshot` is not used: no revision or status moves).
//! * A database older than 39 (33 at the oldest) gets migrations 34 to 39 first, one
//!   transaction each, after the backup. `user_version` then stays 39 until everything is
//!   converted, verified, the old columns are dropped and the database is vacuumed; only
//!   the last write publishes the current version. A database stopped half way is refused by a normal start
//!   and, once the conversion's own tables exist, by this command alike.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

use crate::StorageError;
use crate::db::{FileSizes, file_sizes, suffixed_path};
use crate::migrations::{
    EVENT_BODIES, MESSAGE_ROWS_VERSION, MIGRATIONS, V40_DROP, V40_PREPARE, V41_DROP, V41_PREPARE,
};

use super::compact::{create_backup, integrity_check, parent};
use super::vacuum::rebuild_incremental;
use super::{invalid, offline};
use crate::repo::thread_retention::reclaim::Layout;

mod events;
mod provenance;
mod retention;
mod runs;
mod source;
mod space;
mod verify;

#[cfg(any(test, feature = "unchecked-open"))]
pub mod testing;

pub use events::{EventProblem, EventsReport, KindFigure};
pub use retention::RetentionReport;
pub use source::{Analysis, Problem};
pub use space::VolumeNeed;

pub(crate) use provenance::ProvenancePoint;
pub(crate) use runs::RunPoint;
use runs::{MISMATCH, convert_run};
use source::{PROVENANCE_PROBLEM, Shape, classify, next_run};

pub(crate) use events::EventPoint;

#[derive(Debug, Clone, Default)]
pub struct ConvertBodiesOptions {
    /// Without it the command only reports what it would do.
    pub apply: bool,
    /// Where the verified backup goes (a path that does not exist yet). The default is
    /// `<db>.before-convert-bodies-<unix-ns>.db`.
    pub backup: Option<PathBuf>,
    /// Events converted per transaction (default 1,000; 1 to 100,000). A transaction
    /// also never holds more than 16 MiB of decoded payload.
    pub event_batch_rows: Option<usize>,
}

/// What the command did, or (preview) what it found.
#[derive(Debug, Serialize)]
pub struct ConvertBodiesSummary {
    /// The data this command converts.
    pub scope: &'static str,
    pub source_schema: u32,
    pub target_schema: u32,
    pub preview: bool,
    pub already_converted: bool,
    pub backup: Option<PathBuf>,
    /// `run_contexts` rows when the command started.
    pub source_runs: u64,
    /// Migrations applied first to bring the database to version 39.
    pub migrations_applied: u32,
    pub converted_runs: u64,
    /// Provenance maps moved to rows, and the entry rows they became.
    pub converted_provenance_maps: u64,
    pub provenance_entries_created: u64,
    pub retention: RetentionReport,
    /// Runs that cannot be restored and were stored in the serializer's key order, not
    /// as the old bytes (same JSON value). Their conversation index is built again.
    pub canonicalized_runs: CanonicalizedRuns,
    pub mismatched_runs: u64,
    pub unconverted_runs_remaining: u64,
    pub events_tokenized: u64,
    /// The events step: what it found (preview) or did (`--apply`).
    pub events: EventsReport,
    pub messages_created: u64,
    pub blobs_created: u64,
    pub blob_bytes_created: u64,
    pub references_created: u64,
    /// The most any one volume must have free; see `space_requirements`.
    pub required_free_space_bytes: u64,
    /// What each volume (database, backup, SQLite scratch) must have free.
    pub space_requirements: Vec<VolumeNeed>,
    /// Upper bound of the growth of the database before the VACUUM.
    pub estimated_growth_bytes: u64,
    pub file_bytes_before: FileSizes,
    pub file_bytes_after: FileSizes,
    /// The largest log file after any commit of the preparation, the reclaim and the run
    /// conversions (each commit is checkpointed).
    pub peak_wal_bytes: u64,
    /// The log file after the full VACUUM, which writes the whole database through it.
    pub vacuum_wal_bytes: u64,
    /// Most log frames one run's transaction left, read after its commit (the log
    /// restarts from the start after each checkpoint, so its length is the transaction's).
    pub peak_run_wal_frames: u64,
    /// The same for one provenance map's transaction.
    pub peak_provenance_wal_frames: u64,
    pub foreign_keys_enabled: bool,
    pub foreign_key_check: Option<&'static str>,
    pub integrity_check: Option<&'static str>,
    pub vacuum: bool,
    pub checkpoint_truncated: bool,
    pub auto_vacuum_after: Option<i64>,
    /// Preview only: what a pass that stores nothing found.
    pub analysis: Option<Analysis>,
    pub elapsed_seconds: f64,
    pub stage_elapsed_seconds: BTreeMap<&'static str, f64>,
    pub message: String,
}

#[derive(Debug, Default, Serialize)]
pub struct CanonicalizedRuns {
    pub count: u64,
    pub run_ids: Vec<String>,
}

/// A conversion that stopped. It carries what an operator needs to recover.
#[derive(Debug)]
pub struct ConvertBodiesFailure {
    pub stage: &'static str,
    pub run_id: Option<String>,
    /// A stable code: `mismatch`, `sqlite`, `io`, `refused`.
    pub reason: &'static str,
    pub detail: String,
    pub converted_runs: u64,
    /// The verified backup, once it exists.
    pub backup: Option<PathBuf>,
    /// Whether the database was changed. When it was, the only recovery is the backup.
    pub restore_required: bool,
}

impl fmt::Display for ConvertBodiesFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "convert-bodies failed at stage `{}` ({})",
            self.stage, self.reason
        )?;
        if let Some(run) = &self.run_id {
            write!(formatter, ", run {run}")?;
        }
        write!(formatter, ": {}", self.detail)?;
        write!(
            formatter,
            "\nruns converted before the failure: {}",
            self.converted_runs
        )?;
        match &self.backup {
            Some(backup) => write!(formatter, "\nverified backup: {}", backup.display())?,
            None => write!(formatter, "\nverified backup: none was made")?,
        }
        if self.restore_required {
            write!(
                formatter,
                "\nthe database was changed and is not usable: a normal start and this command refuse it. \
                 Recovery (there is no resume):\n\
                 1. keep every evorch process stopped, and do not start one on this database;\n\
                 2. move the database file and its -wal/-shm files, together, to a safe place for inspection;\n\
                 3. copy the verified backup to the database path (never combine the old -wal/-shm with it);\n\
                 4. check the owner and mode of the restored file;\n\
                 5. run `evorch maintenance convert-bodies` (preview), then with --apply and a new --backup path;\n\
                 6. keep the first backup until the converted database has started and been checked.\n\
                 Do not delete or recreate the .maintenance-lock file."
            )
        } else {
            write!(formatter, "\nthe database was not changed")
        }
    }
}

impl std::error::Error for ConvertBodiesFailure {}

/// Test seams: the points of the command where a test may fail it or damage the data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Point {
    AfterMigrate,
    AfterPrepare,
    AfterRetention,
    /// After the event columns, reference table and accounting triggers were created.
    AfterEventsPrepare,
    Run(RunPoint),
    Event(EventPoint),
    /// After an events batch committed and the log was checkpointed.
    AfterEventBatch {
        frames: u64,
    },
    /// After a run committed and the log was checkpointed; `frames` is how many frames the
    /// run's transaction wrote.
    AfterRun {
        frames: u64,
    },
    Provenance(ProvenancePoint),
    /// After a provenance map committed and the log was checkpointed.
    AfterProvenance {
        frames: u64,
    },
    BeforeVerify,
    BeforeDrop,
    AfterDrop,
    BeforeVacuum,
    BeforePublish,
}

pub(crate) type Hook<'a> = &'a mut dyn FnMut(Point, &Connection) -> Result<(), StorageError>;

/// Converts a version 39 database. See the module documentation for the contract.
///
/// # Errors
/// Fails, without changing the database, when it cannot be converted; fails with a report
/// of the stage, the run and the backup when it stops half way.
pub fn convert_bodies(
    path: &Path,
    options: &ConvertBodiesOptions,
) -> Result<ConvertBodiesSummary, ConvertBodiesFailure> {
    convert_with(path, options, &mut |_, _| Ok(()))
}

struct Progress {
    stage: &'static str,
    run_id: Option<String>,
    converted_runs: u64,
    backup: Option<PathBuf>,
    mutated: bool,
    stages: BTreeMap<&'static str, f64>,
    stage_started: Instant,
}

impl Progress {
    fn enter(&mut self, stage: &'static str) {
        self.leave();
        self.stage = stage;
    }

    fn leave(&mut self) {
        let spent = self.stage_started.elapsed().as_secs_f64();
        *self.stages.entry(self.stage).or_insert(0.0) += spent;
        self.stage_started = Instant::now();
    }

    fn failure(self, error: &StorageError) -> ConvertBodiesFailure {
        let message = match error {
            StorageError::Serialization(message) => message.clone(),
            other => other.to_string(),
        };
        let reason = if message.contains(MISMATCH) {
            "mismatch"
        } else {
            match error {
                StorageError::Sqlite(_) => "sqlite",
                StorageError::Io(_) => "io",
                _ => "refused",
            }
        };
        ConvertBodiesFailure {
            stage: self.stage,
            run_id: self.run_id,
            reason,
            detail: message,
            converted_runs: self.converted_runs,
            backup: self.backup,
            restore_required: self.mutated,
        }
    }
}

/// The log file's size, sampled after each commit: the largest is reported.
struct WalGauge {
    path: PathBuf,
    peak: Cell<u64>,
}

impl WalGauge {
    fn sample(&self) {
        if let Ok(meta) = std::fs::metadata(&self.path) {
            self.peak.set(self.peak.get().max(meta.len()));
        }
    }
}

pub(crate) fn convert_with(
    path: &Path,
    options: &ConvertBodiesOptions,
    hook: Hook<'_>,
) -> Result<ConvertBodiesSummary, ConvertBodiesFailure> {
    let mut progress = Progress {
        stage: "input",
        run_id: None,
        converted_runs: 0,
        backup: None,
        mutated: false,
        stages: BTreeMap::new(),
        stage_started: Instant::now(),
    };
    let started = Instant::now();
    match run(path, options, hook, &mut progress) {
        Ok(mut summary) => {
            progress.leave();
            summary.stage_elapsed_seconds = progress.stages;
            summary.elapsed_seconds = started.elapsed().as_secs_f64();
            Ok(summary)
        }
        Err(error) => Err(progress.failure(&error)),
    }
}

fn refuse_nonempty_wal(path: &Path) -> Result<(), StorageError> {
    let absolute = path
        .canonicalize()
        .map_err(|error| StorageError::Io(error.to_string()))?;
    match std::fs::metadata(suffixed_path(&absolute, "-wal")) {
        Ok(meta) if meta.len() != 0 => Err(invalid(
            "the database has a non-empty WAL, and convert-bodies never recovers one (the \
             recovery would write the database before a backup exists). Stop every evorch \
             process so that the build that wrote the database checkpoints it; or, after an \
             interrupted conversion, restore the verified backup and start over",
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StorageError::Io(error.to_string())),
    }
}

fn exclusive_connection(path: &Path) -> Result<Connection, StorageError> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    // Before the first transaction: the pragma does nothing inside one.
    offline::enable_foreign_keys(&conn)?;
    conn.busy_timeout(Duration::ZERO)?;
    conn.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
    conn.execute_batch("BEGIN EXCLUSIVE; COMMIT;")
        .map_err(|error| {
            invalid(format!(
                "exclusive SQLite lock refused; stop all readers: {error}"
            ))
        })?;
    Ok(conn)
}

fn count(conn: &Connection, sql: &str) -> Result<u64, StorageError> {
    Ok(conn
        .query_row(sql, [], |row| row.get::<_, Option<u64>>(0))?
        .unwrap_or(0))
}

/// `(rows, plain bytes)` of the blob table and the rows of the reference tables.
fn blob_figures(conn: &Connection) -> Result<(u64, u64, u64), StorageError> {
    let blobs = count(conn, "SELECT COUNT(*) FROM content_blobs")?;
    let bytes = count(conn, "SELECT SUM(OCTET_LENGTH(data)) FROM content_blobs")?;
    let mut references = 0;
    for table in crate::repo::content_blobs::ref_tables(conn)? {
        references += count(conn, &format!("SELECT COUNT(*) FROM \"{table}\""))?;
    }
    Ok((blobs, bytes, references))
}

fn basis(conn: &Connection, path: &Path) -> Result<u64, StorageError> {
    let logical: u64 = conn.pragma_query_value(None, "page_count", |row| row.get::<_, u64>(0))?
        * conn.pragma_query_value(None, "page_size", |row| row.get::<_, u64>(0))?;
    Ok(logical.max(file_sizes(path)?.total()))
}

/// Every object of a new database must be in the converted one, with the same SQL.
fn compare_schema(conn: &Connection) -> Result<(), StorageError> {
    fn objects(conn: &Connection) -> Result<BTreeMap<(String, String), String>, StorageError> {
        let mut statement = conn.prepare(
            "SELECT type, name, COALESCE(sql, '') FROM sqlite_master \
             WHERE name NOT LIKE 'sqlite\\_%' ESCAPE '\\'",
        )?;
        let rows = statement.query_map([], |row| {
            let sql: String = row.get(2)?;
            Ok((
                (row.get(0)?, row.get(1)?),
                sql.split_whitespace().collect::<Vec<_>>().join(" "),
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
    let fresh = Connection::open_in_memory()?;
    crate::migrations::apply_migrations(&fresh)?;
    let expected = objects(&fresh)?;
    let found = objects(conn)?;
    for (key, sql) in &expected {
        match found.get(key) {
            None => {
                return Err(invalid(format!(
                    "the converted schema lacks {} {}",
                    key.0, key.1
                )));
            }
            Some(have) if have != sql => {
                return Err(invalid(format!(
                    "the converted schema differs from a new database in {} {}",
                    key.0, key.1
                )));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Checkpoints what is committed and returns the frames the log held (those of the
/// transactions since the last full checkpoint).
fn checkpoint_passive(conn: &Connection) -> Result<u64, StorageError> {
    let (busy, log, done): (i64, i64, i64) =
        conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    if busy != 0 || (log >= 0 && done < log) {
        return Err(invalid(
            "the WAL was not fully checkpointed after a commit; a reader may be open",
        ));
    }
    Ok(u64::try_from(log).unwrap_or(0))
}

fn checkpoint_truncate(conn: &Connection) -> Result<(), StorageError> {
    let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
    if busy != 0 {
        return Err(invalid("the WAL could not be truncated; a reader is open"));
    }
    Ok(())
}

/// What `--apply` refuses before it changes anything, from the full read-only analysis.
fn refusal(analysis: &Analysis) -> Option<String> {
    let mut reasons = Vec::new();
    if analysis.unconvertible_runs > 0 {
        let first: Vec<String> = analysis
            .problems
            .iter()
            .filter(|problem| !problem.reason.starts_with(PROVENANCE_PROBLEM))
            .take(5)
            .map(|problem| format!("run {}: {}", problem.run_id, problem.reason))
            .collect();
        reasons.push(format!(
            "{} runs cannot be converted (first: {})",
            analysis.unconvertible_runs,
            first.join("; ")
        ));
    }
    if analysis.oversized_run_count > 0 {
        reasons.push(format!(
            "{} runs are above {} plain bytes, the most the conversion holds in memory ({})",
            analysis.oversized_run_count,
            source::run_limit(),
            analysis.oversized_runs.join(", ")
        ));
    }
    if analysis.events.unconvertible_events > 0 {
        let first: Vec<String> = analysis
            .events
            .problems
            .iter()
            .filter(|problem| !problem.reason.contains("in-memory limit"))
            .take(5)
            .map(|problem| format!("event {}: {}", problem.id, problem.reason))
            .collect();
        reasons.push(format!(
            "{} events cannot be converted (first: {})",
            analysis.events.unconvertible_events,
            first.join("; ")
        ));
    }
    if analysis.events.oversized_events > 0 {
        let first: Vec<String> = analysis
            .events
            .problems
            .iter()
            .filter(|problem| problem.reason.contains("in-memory limit"))
            .take(5)
            .map(|problem| problem.id.to_string())
            .collect();
        reasons.push(format!(
            "{} events are above {} decoded payload bytes, the most one conversion batch holds (events {})",
            analysis.events.oversized_events,
            events::payload_limit(),
            first.join(", ")
        ));
    }
    if !analysis.reclaim_blockers.is_empty() {
        reasons.push(format!(
            "the reclaim of deleted runs cannot finish ({})",
            analysis.reclaim_blockers.join("; ")
        ));
    }
    if analysis.unconvertible_provenance > 0 {
        reasons.push(format!(
            "{} provenance maps are not typed provenance maps and are never replaced by an \
             empty one (first: {})",
            analysis.unconvertible_provenance,
            analysis
                .problems
                .iter()
                .filter(|problem| problem.reason.starts_with(PROVENANCE_PROBLEM))
                .take(5)
                .map(|problem| format!("run {}", problem.run_id))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    (!reasons.is_empty()).then(|| reasons.join("; "))
}

fn run(
    path: &Path,
    options: &ConvertBodiesOptions,
    hook: Hook<'_>,
    progress: &mut Progress,
) -> Result<ConvertBodiesSummary, StorageError> {
    if !path.is_file() {
        return Err(invalid("database does not exist"));
    }
    if let Some(rows) = options.event_batch_rows
        && !(1..=events::MAX_BATCH_ROWS).contains(&rows)
    {
        return Err(invalid(format!(
            "event_batch_rows must be 1..={}",
            events::MAX_BATCH_ROWS
        )));
    }
    progress.enter("lock");
    let _lease = if options.apply {
        let lease = offline::lease_file(path)?;
        lease.try_lock().map_err(|error| {
            invalid(format!(
                "database is in use; stop all evorch processes: {error}"
            ))
        })?;
        lease
    } else {
        let lease_path = offline::lease_path(path)?;
        if !lease_path.exists() {
            return Err(invalid(format!(
                "preview needs the maintenance lease file {} next to the database. A database \
                 that no build with the lease has run, or a copy made for this conversion, needs \
                 it created first (an empty file, mode 0600)",
                lease_path.display()
            )));
        }
        offline::preview_lease(path)?
    };
    progress.enter("input");
    refuse_nonempty_wal(path)?;
    let (ro, files) = offline::read_only_preview_connection(path)?;
    let shape = classify(&ro)?;
    let source_schema: u32 = ro.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let target_schema =
        u32::try_from(MIGRATIONS.len()).map_err(|_| StorageError::OutOfRange("migration count"))?;
    let sizes_before = file_sizes(path)?;
    let source_runs = count(&ro, "SELECT COUNT(*) FROM run_contexts")?;
    let mut summary = ConvertBodiesSummary {
        scope: "run_contexts, run_context_provenance, events",
        source_schema,
        target_schema,
        preview: !options.apply,
        already_converted: shape == Shape::Converted,
        backup: None,
        source_runs,
        migrations_applied: 0,
        converted_runs: 0,
        converted_provenance_maps: 0,
        provenance_entries_created: 0,
        retention: RetentionReport::default(),
        canonicalized_runs: CanonicalizedRuns::default(),
        mismatched_runs: 0,
        unconverted_runs_remaining: 0,
        events_tokenized: 0,
        events: EventsReport::default(),
        messages_created: 0,
        blobs_created: 0,
        blob_bytes_created: 0,
        references_created: 0,
        required_free_space_bytes: basis(&ro, path)?.saturating_mul(4),
        space_requirements: Vec::new(),
        estimated_growth_bytes: 0,
        file_bytes_before: sizes_before,
        file_bytes_after: sizes_before,
        peak_wal_bytes: 0,
        vacuum_wal_bytes: 0,
        peak_run_wal_frames: 0,
        peak_provenance_wal_frames: 0,
        foreign_keys_enabled: false,
        foreign_key_check: None,
        integrity_check: None,
        vacuum: false,
        checkpoint_truncated: false,
        auto_vacuum_after: None,
        analysis: None,
        elapsed_seconds: 0.0,
        stage_elapsed_seconds: BTreeMap::new(),
        message: String::new(),
    };
    if shape == Shape::Converted {
        files.verify()?;
        summary.message =
            "the database is already in the current format; nothing was written".into();
        return Ok(summary);
    }
    progress.enter("analyze");
    let mut analysis = match shape {
        Shape::Source => source::analyze(&ro)?,
        Shape::RunsConverted => source::analyze_runs_converted(&ro)?,
        Shape::EventsPending | Shape::Converted => source::measure_converted(&ro)?,
    };
    analysis.events = events::analyze(&ro)?;
    analysis.estimated_growth_bytes = analysis
        .estimated_growth_bytes
        .saturating_add(analysis.events.estimated_growth_bytes);
    let basis = basis(&ro, path)?;
    let backup_directory = options.backup.as_deref().map_or(parent(path), parent);
    let scratch_directory = space::scratch_directory();
    let needs = space::plan(
        &space::place(parent(path))?,
        &space::place(backup_directory)?,
        &space::place(&scratch_directory)?,
        basis,
        analysis.estimated_growth_bytes,
    );
    summary.estimated_growth_bytes = analysis.estimated_growth_bytes;
    summary.required_free_space_bytes = needs.iter().map(|need| need.bytes).max().unwrap_or(0);
    summary.space_requirements = needs.clone();
    files.verify()?;
    let refusal = refusal(&analysis);
    if !options.apply {
        summary.message = match refusal {
            Some(_) => "preview found data that cannot be converted; --apply would refuse".into(),
            None => "preview only; rerun with --apply to convert (a verified backup is made first)"
                .into(),
        };
        summary.analysis = Some(analysis);
        return Ok(summary);
    }
    if let Some(reason) = refusal {
        return Err(invalid(format!(
            "nothing was converted and nothing was changed: {reason}"
        )));
    }
    space::require(&needs)?;
    summary.analysis = Some(analysis);
    drop(ro);

    progress.enter("lock");
    let conn = exclusive_connection(path)?;
    summary.foreign_keys_enabled = true;
    if classify(&conn)? != shape {
        return Err(invalid("the database changed while the conversion started"));
    }
    let convert_runs = shape == Shape::Source;
    let convert_provenance = shape != Shape::EventsPending;
    let layout = if convert_runs {
        Layout::Converting
    } else {
        Layout::Current
    };
    let gauge = WalGauge {
        path: suffixed_path(
            &path
                .canonicalize()
                .map_err(|e| StorageError::Io(e.to_string()))?,
            "-wal",
        ),
        peak: Cell::new(0),
    };

    progress.enter("backup");
    let backup = create_backup(
        &conn,
        path,
        options.backup.as_deref(),
        "before-convert-bodies",
        basis,
    )?;
    progress.backup = Some(backup.clone());
    summary.backup = Some(backup);

    progress.mutated = true;
    if convert_runs && source_schema < MESSAGE_ROWS_VERSION - 1 {
        // The database is older than version 39: the ordinary migrations, in their own
        // transactions, after the backup, then the conversion proper. A stop in between
        // leaves a schema that is valid at its own version, but the way back is the same:
        // restore the backup.
        progress.enter("migrate");
        crate::migrations::apply_up_to(&conn, MESSAGE_ROWS_VERSION - 1)?;
        summary.migrations_applied = MESSAGE_ROWS_VERSION - 1 - source_schema;
        checkpoint_passive(&conn)?;
        gauge.sample();
        if classify(&conn)? != Shape::Source {
            return Err(invalid(
                "the database is not a regular version 39 database after its migrations",
            ));
        }
        hook(Point::AfterMigrate, &conn)?;
    }
    if convert_provenance {
        progress.enter("prepare");
        conn.execute_batch("BEGIN IMMEDIATE")?;
        if convert_runs {
            conn.execute_batch(V40_PREPARE)?;
        }
        conn.execute_batch(V41_PREPARE)?;
        conn.execute_batch("COMMIT")?;
        checkpoint_passive(&conn)?;
        gauge.sample();
        hook(Point::AfterPrepare, &conn)?;
    }
    progress.enter("prepare_events");
    conn.execute_batch("BEGIN IMMEDIATE")?;
    conn.execute_batch(EVENT_BODIES)?;
    conn.execute_batch("COMMIT")?;
    checkpoint_passive(&conn)?;
    gauge.sample();
    hook(Point::AfterEventsPrepare, &conn)?;

    progress.enter("retention");
    summary.retention = retention::reclaim_deleted(&conn, layout, &mut || gauge.sample())?;
    checkpoint_passive(&conn)?;
    hook(Point::AfterRetention, &conn)?;
    let (blobs_before, blob_bytes_before, references_before) = blob_figures(&conn)?;
    let (kept_runs, fingerprint_before) = verify::metadata_fingerprint(&conn)?;
    let mut peak_event_frames = 0_u64;

    progress.enter("convert");
    if convert_runs {
        let mut cursor: Option<String> = None;
        while let Some(run_id) = next_run(&conn, cursor.as_deref(), false, false)? {
            progress.run_id = Some(run_id.clone());
            let converted = convert_run(&conn, &run_id, &mut |point, conn| {
                hook(Point::Run(point), conn)
            })?;
            let frames = checkpoint_passive(&conn)?;
            gauge.sample();
            summary.peak_run_wal_frames = summary.peak_run_wal_frames.max(frames);
            progress.converted_runs += 1;
            summary.messages_created += converted.messages;
            if converted.canonicalized {
                summary.canonicalized_runs.count += 1;
                summary.canonicalized_runs.run_ids.push(run_id.clone());
            }
            hook(Point::AfterRun { frames }, &conn)?;
            cursor = Some(run_id);
        }
        progress.run_id = None;
        summary.converted_runs = progress.converted_runs;
        if summary.converted_runs != kept_runs {
            return Err(invalid(format!(
                "{} runs were converted, {kept_runs} were expected",
                summary.converted_runs
            )));
        }
    }

    if convert_provenance {
        progress.enter("provenance");
        let mut cursor: Option<String> = None;
        while let Some(run_id) = provenance::next_row(&conn, cursor.as_deref())? {
            progress.run_id = Some(run_id.clone());
            let entries = provenance::convert_row(&conn, &run_id, &mut |point, conn| {
                hook(Point::Provenance(point), conn)
            })?;
            let frames = checkpoint_passive(&conn)?;
            gauge.sample();
            summary.peak_provenance_wal_frames = summary.peak_provenance_wal_frames.max(frames);
            summary.converted_provenance_maps += 1;
            summary.provenance_entries_created += entries;
            hook(Point::AfterProvenance { frames }, &conn)?;
            cursor = Some(run_id);
        }
        progress.run_id = None;
    }

    progress.enter("convert_events");
    let events_identity = events::identity_fingerprint(&conn)?;
    let events_accounting = events::accounting(&conn)?;
    summary.events.payload_stored_bytes_before =
        count(&conn, "SELECT SUM(OCTET_LENGTH(payload)) FROM events")?;
    summary.events.accounted_bytes_before = events_accounting.sessions;
    let batch_rows = options
        .event_batch_rows
        .unwrap_or(events::DEFAULT_BATCH_ROWS);
    events::convert_all(
        &conn,
        batch_rows,
        &mut summary.events,
        &mut |point, conn| match point {
            EventPoint::AfterBatch { .. } => {
                let frames = checkpoint_passive(conn)?;
                gauge.sample();
                peak_event_frames = peak_event_frames.max(frames);
                hook(Point::AfterEventBatch { frames }, conn)
            }
            other => hook(Point::Event(other), conn),
        },
    )?;
    summary.events.peak_batch_wal_frames = peak_event_frames;
    summary.events_tokenized = summary.events.converted_events;
    let (blobs_after, blob_bytes_after, references_after) = blob_figures(&conn)?;
    summary.blobs_created = blobs_after.saturating_sub(blobs_before);
    summary.blob_bytes_created = blob_bytes_after.saturating_sub(blob_bytes_before);
    summary.references_created = references_after.saturating_sub(references_before);
    summary.events.references_created = summary.references_created;

    progress.enter("verify");
    hook(Point::BeforeVerify, &conn)?;
    verify::verify_all(&conn, convert_runs)?;
    events::verify(
        &conn,
        events_identity,
        events_accounting,
        &mut summary.events,
    )?;
    summary.events.payload_stored_bytes_after =
        count(&conn, "SELECT SUM(OCTET_LENGTH(payload)) FROM events")?;
    summary.foreign_key_check = Some("ok");
    summary.unconverted_runs_remaining = 0;
    let (runs_after, fingerprint_after) = verify::metadata_fingerprint(&conn)?;
    if runs_after != kept_runs || fingerprint_after != fingerprint_before {
        return Err(invalid(
            "the identity, times, revisions or flags of a run changed during the conversion",
        ));
    }

    progress.enter("drop_column");
    hook(Point::BeforeDrop, &conn)?;
    conn.execute_batch("BEGIN IMMEDIATE")?;
    if convert_runs {
        conn.execute_batch(V40_DROP)?;
    }
    if convert_provenance {
        conn.execute_batch(V41_DROP)?;
    }
    conn.execute_batch("COMMIT")?;
    hook(Point::AfterDrop, &conn)?;
    compare_schema(&conn)?;

    summary.peak_wal_bytes = gauge.peak.get();
    gauge.peak.set(0);
    progress.enter("vacuum");
    hook(Point::BeforeVacuum, &conn)?;
    super::compact::collect_unreferenced_blobs(&conn)?;
    space::require(&space::plan_vacuum(
        &space::place(parent(path))?,
        &space::place(&scratch_directory)?,
        self::basis(&conn, path)?,
    ))?;
    summary.auto_vacuum_after = Some(rebuild_incremental(&conn)?);
    summary.vacuum = true;
    gauge.sample();
    summary.vacuum_wal_bytes = gauge.peak.get();
    checkpoint_truncate(&conn)?;
    integrity_check(&conn)?;
    summary.integrity_check = Some("ok");
    offline::require_no_foreign_key_violations(&conn)?;

    progress.enter("publish");
    hook(Point::BeforePublish, &conn)?;
    conn.execute_batch("BEGIN IMMEDIATE")?;
    conn.pragma_update(None, "user_version", target_schema)?;
    conn.execute_batch("COMMIT")?;
    crate::migrations::require_current_format(&conn)?;
    checkpoint_truncate(&conn)?;
    summary.checkpoint_truncated = true;
    summary.file_bytes_after = file_sizes(path)?;
    summary.message =
        "converted; keep the backup until the converted database has started and been checked"
            .into();
    Ok(summary)
}

#[cfg(test)]
mod events_tests;
#[cfg(test)]
mod tests;
