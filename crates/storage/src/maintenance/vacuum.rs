//! One-time offline migration of a legacy `auto_vacuum=NONE` database to
//! INCREMENTAL. SQLite applies the mode only through a full VACUUM, so startup
//! never does it; this command and `compact-events` are the explicit paths.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use rusqlite::{Connection, OpenFlags};

use super::compact::{check_schema, create_backup, integrity_check, parent};
use super::{invalid, offline};
use crate::StorageError;
use crate::db::FileSizes;

/// `PRAGMA auto_vacuum` value for INCREMENTAL.
pub(super) const AUTO_VACUUM_INCREMENTAL: i64 = 2;

#[derive(Debug, Clone, Default)]
pub struct VacuumOptions {
    pub apply: bool,
    pub backup: Option<PathBuf>,
}

#[derive(Debug, serde::Serialize)]
pub struct VacuumSummary {
    pub preview: bool,
    /// `PRAGMA auto_vacuum` as found (0 NONE, 1 FULL, 2 INCREMENTAL).
    pub auto_vacuum_before: i64,
    /// Actual state: equal to `auto_vacuum_before` in preview and when nothing
    /// needed doing; 2 after a successful apply.
    pub auto_vacuum_after: i64,
    /// The database already is INCREMENTAL; no backup, VACUUM or write happens.
    pub already_incremental: bool,
    pub backup: Option<PathBuf>,
    pub required_free_space_bytes: u64,
    pub vacuum: bool,
    pub checkpoint_truncated: bool,
    pub integrity_check: Option<String>,
    pub file_bytes_before: FileSizes,
    pub file_bytes_after: FileSizes,
    pub elapsed_seconds: f64,
    pub message: String,
}

pub(super) fn auto_vacuum_mode(conn: &Connection) -> Result<i64, StorageError> {
    Ok(conn.pragma_query_value(None, "auto_vacuum", |row| row.get(0))?)
}

/// Set INCREMENTAL, run the full VACUUM that makes SQLite apply it, and verify
/// the mode reads back as INCREMENTAL. Caller holds the exclusive lease.
pub(super) fn rebuild_incremental(conn: &Connection) -> Result<i64, StorageError> {
    conn.pragma_update(None, "auto_vacuum", AUTO_VACUUM_INCREMENTAL)?;
    conn.execute_batch("VACUUM;")?;
    let mode = auto_vacuum_mode(conn)?;
    if mode != AUTO_VACUUM_INCREMENTAL {
        return Err(invalid(format!(
            "VACUUM finished but auto_vacuum is {mode}, expected {AUTO_VACUUM_INCREMENTAL}"
        )));
    }
    Ok(mode)
}

/// Preview is read-only and refuses a live store or nonempty WAL. Apply holds
/// the exclusive lease and SQLite locking mode, takes a verified backup like
/// `compact-events`, then VACUUMs with INCREMENTAL requested. A database that
/// is already INCREMENTAL is left untouched.
pub fn vacuum_database(
    path: &Path,
    options: &VacuumOptions,
) -> Result<VacuumSummary, StorageError> {
    let start = Instant::now();
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
    check_schema(&conn, 1)?;
    let mode = auto_vacuum_mode(&conn)?;
    let sizes = crate::db::file_sizes(path)?;
    let logical: u64 = conn.pragma_query_value(None, "page_count", |row| row.get::<_, u64>(0))?
        * conn.pragma_query_value(None, "page_size", |row| row.get::<_, u64>(0))?;
    let basis = logical.max(sizes.total());
    let already_incremental = mode == AUTO_VACUUM_INCREMENTAL;
    let mut summary = VacuumSummary {
        preview: !options.apply,
        auto_vacuum_before: mode,
        auto_vacuum_after: mode,
        already_incremental,
        backup: None,
        required_free_space_bytes: basis.saturating_mul(4),
        vacuum: false,
        checkpoint_truncated: false,
        integrity_check: None,
        file_bytes_before: sizes,
        file_bytes_after: sizes,
        elapsed_seconds: 0.0,
        message: String::new(),
    };
    if already_incremental {
        summary.message = "auto_vacuum is already INCREMENTAL; nothing to do".into();
    } else if !options.apply {
        summary.message =
            "auto_vacuum is not INCREMENTAL; rerun with --apply to switch it with one full VACUUM"
                .into();
    }
    if already_incremental || !options.apply {
        if let Some(files) = files {
            files.verify()?;
        }
        summary.elapsed_seconds = start.elapsed().as_secs_f64();
        return Ok(summary);
    }
    offline::require_space(parent(path), basis.saturating_mul(4))?;
    summary.backup = Some(create_backup(
        &conn,
        path,
        options.backup.as_deref(),
        "before-vacuum",
        basis,
    )?);
    offline::require_space(parent(path), basis.saturating_mul(2))?;
    summary.auto_vacuum_after = rebuild_incremental(&conn)?;
    summary.vacuum = true;
    let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
    if busy != 0 {
        return Err(invalid(
            "vacuum committed but WAL truncate busy; rerun offline",
        ));
    }
    summary.checkpoint_truncated = true;
    integrity_check(&conn)?;
    summary.integrity_check = Some("ok".into());
    summary.file_bytes_after = crate::db::file_sizes(path)?;
    summary.message = "auto_vacuum switched to INCREMENTAL".into();
    summary.elapsed_seconds = start.elapsed().as_secs_f64();
    Ok(summary)
}
