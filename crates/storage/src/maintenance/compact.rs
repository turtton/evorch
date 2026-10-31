//! Explicit, restartable offline compaction. Never migrate the source implicitly.

use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OpenFlags};

use super::{invalid, offline, vacuum};
use crate::{Database, EventBackfillOptions, EventBackfillSummary, StorageError};

mod deltas;
mod sidebar;
mod statistics;

pub use statistics::{CompressionSample, Snapshot, Stage};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DeltaMode {
    #[default]
    Preserve,
    Delete,
}

#[derive(Debug, Clone)]
pub struct CompactOptions {
    pub apply: bool,
    pub backup: Option<PathBuf>,
    pub batch_rows: usize,
    pub provider_retention_days: u64,
    pub delta_mode: DeltaMode,
    /// GUI sidebar containing out-of-DB user-message replay boundaries.
    /// Direct API callers default to the database's sibling sidebar.json.
    pub sidebar: Option<PathBuf>,
}

impl Default for CompactOptions {
    fn default() -> Self {
        Self {
            apply: false,
            backup: None,
            batch_rows: 1_000,
            provider_retention_days: 90,
            delta_mode: DeltaMode::Preserve,
            sidebar: None,
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct CompactSummary {
    pub preview: bool,
    pub before: Snapshot,
    pub after: Snapshot,
    /// Exact row-count projection; preview before/after remain actual snapshots.
    pub projected_event_rows: std::collections::BTreeMap<String, u64>,
    pub backfill: EventBackfillSummary,
    pub noise: Stage,
    pub message_deltas: Stage,
    pub reasoning_deltas: Stage,
    pub provider_observations: Stage,
    pub event_compression: CompressionSample,
    pub context_compression: CompressionSample,
    pub compressed_event_rows: u64,
    pub compressed_context_values: u64,
    /// Existing individual rows over 64 KiB cannot be split without changing IDs.
    pub oversized_rows: u64,
    pub backup: Option<PathBuf>,
    pub required_free_space_bytes: u64,
    pub vacuum: bool,
    /// `PRAGMA auto_vacuum` as found (0 NONE, 1 FULL, 2 INCREMENTAL).
    pub auto_vacuum_before: i64,
    /// Like `after`, actual state: equal to `auto_vacuum_before` in preview;
    /// apply sets INCREMENTAL before VACUUM and verifies it reads back as 2.
    pub auto_vacuum_after: i64,
    pub checkpoint_truncated: bool,
    pub integrity_check: Option<String>,
    pub elapsed_seconds: f64,
    pub sidebar: PathBuf,
    pub sidebar_boundary_count: usize,
    pub delta_aggregation_enabled: bool,
    pub warnings: Vec<String>,
}

/// Lightweight online backfill: no runtime, migrations, backup or vacuum.
/// A shared writer lease excludes concurrent offline maintenance.
pub fn backfill_events(
    path: &Path,
    options: &EventBackfillOptions,
) -> Result<EventBackfillSummary, StorageError> {
    validate_batch(options.batch_rows)?;
    if !path.is_file() {
        return Err(invalid("database does not exist"));
    }
    let _lease = offline::writer_lease(path)?;
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    check_schema(&conn, 30)?;
    Database { conn }.backfill_event_columns(options)
}

fn validate_batch(rows: usize) -> Result<(), StorageError> {
    if rows == 0 || rows > 100_000 {
        return Err(invalid("batch_rows must be 1..=100000"));
    }
    Ok(())
}

pub(super) fn check_schema(conn: &Connection, minimum: u32) -> Result<(), StorageError> {
    let version: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version < minimum || version as usize > crate::migrations::MIGRATIONS.len() {
        return Err(invalid(
            "unsupported maintenance schema; use current evorch to migrate first",
        ));
    }
    Ok(())
}

/// Preview is immutable/read-only and refuses a live store or nonempty WAL.
/// Apply holds both an exclusive lease and SQLite locking mode throughout.
/// Every delta group is atomic; every other rewrite uses bounded transactions.
pub fn compact_events(
    path: &Path,
    options: &CompactOptions,
) -> Result<CompactSummary, StorageError> {
    let start = Instant::now();
    validate_batch(options.batch_rows)?;
    let cutoff = provider_cutoff(options.provider_retention_days)?;
    if !path.is_file() {
        return Err(invalid("database does not exist"));
    }
    let _lease = if options.apply {
        let lease = offline::lease_file(path)?;
        lease.try_lock().map_err(|error| {
            invalid(format!(
                "database is in use; stop all evorch processes: {error}"
            ))
        })?;
        lease
    } else {
        offline::preview_lease(path)?
    };
    let (conn, files) = if options.apply {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        conn.busy_timeout(Duration::ZERO)?;
        conn.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
        conn.execute_batch("BEGIN EXCLUSIVE; COMMIT;")
            .map_err(|error| {
                invalid(format!(
                    "exclusive SQLite lock refused; stop all readers: {error}"
                ))
            })?;
        (conn, None)
    } else {
        let (conn, files) = offline::read_only_preview_connection(path)?;
        (conn, Some(files))
    };
    check_schema(&conn, 31)?;
    let db = Database { conn };
    let before = statistics::snapshot(&db.conn, path)?;
    let logical: u64 = db
        .conn
        .pragma_query_value(None, "page_count", |row| row.get::<_, u64>(0))?
        * db.conn
            .pragma_query_value(None, "page_size", |row| row.get::<_, u64>(0))?;
    let basis = logical.max(before.file_bytes.total());
    let auto_vacuum = vacuum::auto_vacuum_mode(&db.conn)?;
    let sidebar_path = options
        .sidebar
        .clone()
        .unwrap_or_else(|| parent(path).join("sidebar.json"));
    let boundaries = sidebar::boundaries(&sidebar_path);
    let warnings = if options.delta_mode == DeltaMode::Preserve {
        boundaries.as_ref().err().map(|reason| format!(
            "Delta aggregation disabled: {reason} ({}); supply the GUI's sidebar with --sidebar. Other compaction stages remain enabled.",
            sidebar_path.display()
        )).into_iter().collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    for warning in &warnings {
        eprintln!("WARNING: {warning}");
    }
    let boundaries = boundaries.ok();
    let mut summary = CompactSummary {
        preview: !options.apply,
        after: before.clone(),
        before,
        projected_event_rows: Default::default(),
        backfill: EventBackfillSummary::default(),
        noise: Stage::default(),
        message_deltas: Stage::default(),
        reasoning_deltas: Stage::default(),
        provider_observations: Stage::default(),
        event_compression: CompressionSample::default(),
        context_compression: CompressionSample::default(),
        compressed_event_rows: 0,
        compressed_context_values: 0,
        oversized_rows: 0,
        backup: None,
        required_free_space_bytes: basis.saturating_mul(4),
        vacuum: false,
        auto_vacuum_before: auto_vacuum,
        auto_vacuum_after: auto_vacuum,
        checkpoint_truncated: false,
        integrity_check: None,
        elapsed_seconds: 0.0,
        sidebar: sidebar_path,
        sidebar_boundary_count: boundaries.as_ref().map_or(0, Vec::len),
        delta_aggregation_enabled: options.delta_mode == DeltaMode::Preserve
            && boundaries.is_some(),
        warnings,
    };
    if options.apply {
        let parent = parent(path);
        offline::require_space(parent, basis.saturating_mul(4))?;
        summary.backup = Some(create_backup(
            &db.conn,
            path,
            options.backup.as_deref(),
            "before-compact-events",
            basis,
        )?);
        summary.backfill = db.backfill_event_columns(&EventBackfillOptions {
            batch_rows: options.batch_rows,
            max_rows: None,
        })?;
    } else {
        summary.backfill.remaining = db.unbackfilled_event_count()?;
    }
    summary.noise = delete_matching(&db.conn, options, None)?;
    let mut virtual_deltas = (!options.apply).then(statistics::EventSample::default);
    let (message, oversized) = deltas::compact_preview(
        &db.conn,
        options,
        false,
        cutoff,
        virtual_deltas.as_mut(),
        boundaries.as_deref(),
    )?;
    summary.message_deltas = message;
    summary.oversized_rows += oversized;
    let (reasoning, oversized) = deltas::compact_preview(
        &db.conn,
        options,
        true,
        cutoff,
        virtual_deltas.as_mut(),
        boundaries.as_deref(),
    )?;
    summary.reasoning_deltas = reasoning;
    summary.oversized_rows += oversized;
    if let Some(cutoff) = cutoff {
        summary.provider_observations = delete_matching(&db.conn, options, Some(cutoff))?;
    }
    let (sample, rows) = statistics::compress_events(&db.conn, options, virtual_deltas)?;
    summary.event_compression = sample;
    summary.compressed_event_rows = rows;
    let (sample, values) = statistics::compress_contexts(&db.conn, options)?;
    summary.context_compression = sample;
    summary.compressed_context_values = values;
    if options.apply {
        recalculate_accounting(&db.conn)?;
        offline::require_space(parent(path), basis.saturating_mul(2))?;
        // Existing databases stay auto_vacuum=NONE unless the rebuild also
        // switches the mode; this is the only place that is safe to do so.
        summary.auto_vacuum_after = vacuum::rebuild_incremental(&db.conn)?;
        summary.vacuum = true;
        let busy: i64 = db
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        if busy != 0 {
            return Err(invalid(
                "compaction committed but WAL truncate busy; rerun offline",
            ));
        }
        summary.checkpoint_truncated = true;
        integrity_check(&db.conn)?;
        summary.integrity_check = Some("ok".into());
        summary.after = statistics::snapshot(&db.conn, path)?;
        summary.projected_event_rows = summary
            .after
            .events
            .iter()
            .map(|(kind, count)| (kind.clone(), count.rows))
            .collect();
    } else {
        // File size is deliberately not predicted from logical payload bytes:
        // page packing, indexes and unrelated tables also determine it.
        let mut projected = summary.before.events.clone();
        statistics::subtract_deleted(&mut projected, &summary.noise);
        statistics::subtract_deleted(&mut projected, &summary.message_deltas);
        statistics::subtract_deleted(&mut projected, &summary.reasoning_deltas);
        statistics::subtract_deleted(&mut projected, &summary.provider_observations);
        summary.projected_event_rows = projected
            .into_iter()
            .map(|(kind, count)| (kind, count.rows))
            .collect();
        files.as_ref().expect("preview files").verify()?;
    }
    summary.elapsed_seconds = start.elapsed().as_secs_f64();
    Ok(summary)
}

pub(super) fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// Verified `VACUUM INTO` backup shared by the offline maintenance commands.
/// The default name is `<db>.<label>-<unix-ns>.db`; an explicit one must be new.
pub(super) fn create_backup(
    conn: &Connection,
    path: &Path,
    requested: Option<&Path>,
    label: &str,
    basis: u64,
) -> Result<PathBuf, StorageError> {
    let backup = requested.map(Path::to_path_buf).unwrap_or_else(|| {
        let ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut name = path.as_os_str().to_os_string();
        name.push(format!(".{label}-{ns}.db"));
        name.into()
    });
    offline::require_space(parent(&backup), basis.saturating_mul(4))?;
    backup_database(conn, &backup)?;
    // Emit the path before the first source mutation, including interrupted runs.
    eprintln!("Verified maintenance backup: {}", backup.display());
    Ok(backup)
}

fn backup_database(conn: &Connection, path: &Path) -> Result<(), StorageError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    drop(
        options
            .open(path)
            .map_err(|error| StorageError::Io(error.to_string()))?,
    );
    conn.execute(
        "VACUUM INTO ?1",
        [path
            .to_str()
            .ok_or_else(|| invalid("backup path must be UTF-8"))?],
    )?;
    let backup = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    integrity_check(&backup)?;
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| StorageError::Io(error.to_string()))?;
    Ok(())
}

pub(super) fn integrity_check(conn: &Connection) -> Result<(), StorageError> {
    let result: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if result != "ok" {
        return Err(invalid(format!("integrity_check failed: {result}")));
    }
    Ok(())
}

fn provider_cutoff(days: u64) -> Result<Option<i64>, StorageError> {
    if days == 0 {
        return Ok(None);
    }
    let duration = Duration::from_secs(
        days.checked_mul(86_400)
            .ok_or(StorageError::OutOfRange("provider retention days"))?,
    );
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StorageError::OutOfRange("wall clock"))?;
    // A retention interval longer than the epoch's age expires nothing.
    Ok(Some(
        i64::try_from(now.saturating_sub(duration).as_nanos())
            .map_err(|_| StorageError::OutOfRange("provider retention cutoff"))?,
    ))
}

fn delete_matching(
    conn: &Connection,
    options: &CompactOptions,
    cutoff: Option<i64>,
) -> Result<Stage, StorageError> {
    let mut stage = Stage::default();
    let mut after = i64::MIN;
    loop {
        // Decode NULL-subkind legacy rows in preview; apply has backfilled them.
        let filter = if cutoff.is_some() {
            "kind = 'Provider' AND wall_clock_ns < ?3"
        } else {
            "kind IN ('Fault','Lifecycle')"
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT id, kind, subkind, payload, OCTET_LENGTH(payload),session_id FROM events WHERE id > ?1 AND {filter} ORDER BY id LIMIT ?2"
        ))?;
        let mut rows = stmt.query(rusqlite::params_from_iter(
            [after, options.batch_rows as i64].into_iter().chain(cutoff),
        ))?;
        let mut ids = Vec::new();
        let mut scanned = 0;
        while let Some(row) = rows.next()? {
            scanned += 1;
            after = row.get(0)?;
            let kind: String = row.get(1)?;
            let subkind = statistics::subkind(row.get(2)?, row.get_ref(3)?);
            let matched = if cutoff.is_some() {
                matches!(
                    subkind.as_str(),
                    "RequestCompleted" | "FirstTokenObserved" | "CacheReuseObserved"
                )
            } else {
                (kind == "Fault" && subkind == "SubscriberLagged")
                    || (kind == "Lifecycle" && subkind == "RunProgress")
            };
            if matched {
                let bytes = row.get::<_, u64>(4)?;
                stage.deleted(&kind, &subkind, bytes);
                ids.push((after, row.get::<_, Option<String>>(5)?, bytes));
            }
        }
        drop(rows);
        drop(stmt);
        if options.apply && !ids.is_empty() {
            let tx = conn.unchecked_transaction()?;
            for (id, session, bytes) in ids {
                tx.execute("DELETE FROM events WHERE id=?1", [id])?;
                adjust_session_bytes(&tx, session.as_deref(), bytes, 0)?;
            }
            tx.commit()?;
            checkpoint(conn)?;
        }
        if scanned == 0 {
            break;
        }
    }
    Ok(stage)
}

fn checkpoint(conn: &Connection) -> Result<(), StorageError> {
    conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))?;
    Ok(())
}

/// events' triggers maintain the day/session indexes, but sessions' cached
/// total is application-owned. Update it in the SAME transaction as each
/// committed rewrite, not only in the final reconciliation.
fn adjust_session_bytes(
    conn: &Connection,
    session: Option<&str>,
    before: u64,
    after: u64,
) -> Result<(), StorageError> {
    if let Some(session) = session {
        conn.execute(
            "UPDATE sessions SET total_event_bytes=MAX(0,total_event_bytes-?2+?3) WHERE id=?1",
            rusqlite::params![
                session,
                i64::try_from(before)
                    .map_err(|_| StorageError::OutOfRange("session payload bytes"))?,
                i64::try_from(after)
                    .map_err(|_| StorageError::OutOfRange("session payload bytes"))?
            ],
        )?;
    }
    Ok(())
}

fn recalculate_accounting(conn: &Connection) -> Result<(), StorageError> {
    conn.execute_batch(
        "BEGIN IMMEDIATE;
         DELETE FROM event_session_bytes;
         INSERT INTO event_session_bytes SELECT session_id, SUM(OCTET_LENGTH(payload)) FROM events WHERE session_id IS NOT NULL GROUP BY session_id;
         DELETE FROM event_day_bytes;
         INSERT INTO event_day_bytes SELECT wall_clock_ns/86400000000000-(wall_clock_ns%86400000000000<0), SUM(OCTET_LENGTH(payload)) FROM events GROUP BY 1;
         UPDATE sessions SET total_event_bytes=COALESCE((SELECT payload_bytes FROM event_session_bytes WHERE session_id=sessions.id),0);
         COMMIT;"
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
