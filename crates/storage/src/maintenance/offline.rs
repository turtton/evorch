//! Offline cursor-only rewrite. Never use Database::open here: it migrates.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, params};

use super::invalid;
use crate::StorageError;

const FILTER: &str = "kind = 'Orchestrator' AND json_valid(payload) AND json_extract(payload, '$.payload.kind') = 'TaskProgressed' AND json_type(payload, '$.payload.payload.progress.resume_cursor') IS NOT NULL";
const CURSOR: &str = "$.payload.payload.progress.resume_cursor";

#[derive(Debug, Clone)]
pub struct StripOptions {
    pub apply: bool,
    pub backup: Option<PathBuf>,
    pub no_backup: bool,
    pub no_vacuum: bool,
    pub batch_size: usize,
}

impl Default for StripOptions {
    fn default() -> Self {
        Self {
            apply: false,
            backup: None,
            no_backup: false,
            no_vacuum: false,
            batch_size: 32,
        }
    }
}

#[derive(Debug, Default, serde::Serialize)]
pub struct StripSummary {
    pub dry_run: bool,
    pub target_rows: u64,
    pub payload_bytes: u64,
    pub resume_cursor_bytes: u64,
    pub estimated_payload_reduction_bytes: u64,
    pub rewritten_rows: u64,
    pub backup: Option<PathBuf>,
    pub database_bytes_before: u64,
    pub database_bytes_after: u64,
    pub vacuum: bool,
    pub checkpoint_truncated: bool,
    pub vacuum_space_budget_bytes: u64,
}

/// New application writers share this lease for their entire lifetime. Its
/// pathname is stable and never unlinked (unlink/recreate would split the lock).
/// SQLite locking below also excludes older/non-cooperating active connections.
pub(crate) fn writer_lease(path: &Path) -> Result<File, StorageError> {
    let file = lease_file(path)?;
    file.try_lock_shared()
        .map_err(|error| invalid(format!("database is under offline maintenance: {error}")))?;
    Ok(file)
}

pub(super) fn lease_path(path: &Path) -> Result<PathBuf, StorageError> {
    let canonical = path
        .canonicalize()
        .or_else(|_| {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            Ok::<_, std::io::Error>(
                parent
                    .canonicalize()?
                    .join(path.file_name().unwrap_or_default()),
            )
        })
        .map_err(|error| StorageError::Io(error.to_string()))?;
    let mut name = canonical.into_os_string();
    name.push(".maintenance-lock");
    Ok(name.into())
}

pub(super) fn lease_file(path: &Path) -> Result<File, StorageError> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(lease_path(path)?)
        .map_err(|error| StorageError::Io(error.to_string()))
}

pub(super) fn preview_lease(path: &Path) -> Result<File, StorageError> {
    let lease = match File::open(lease_path(path)?) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(invalid(
                "offline preview requires an existing maintenance lease; start current evorch once and exit cleanly first",
            ));
        }
        Err(error) => return Err(StorageError::Io(error.to_string())),
    };
    lease.try_lock().map_err(|error| {
        invalid(format!(
            "database is in use; stop all evorch processes: {error}"
        ))
    })?;
    Ok(lease)
}

/// Preview opens the source read-only; even its lease is opened read-only and
/// only if already present (otherwise refuses). No sidecar, backup, migration
/// or checkpoint is made. A nonempty WAL also refuses before SQLite is opened.
/// Apply holds the lease and SQLite exclusive locking mode across all commits,
/// backup and vacuum. A plain BEGIN EXCLUSIVE in WAL is *not* sufficient.
pub fn strip_task_progress_cursor(
    path: &Path,
    options: &StripOptions,
) -> Result<StripSummary, StorageError> {
    if !(1..=256).contains(&options.batch_size) || options.no_backup && options.backup.is_some() {
        return Err(invalid(
            "batch_size must be 1..=256; --backup conflicts with --no-backup",
        ));
    }
    if !path.is_file() {
        return Err(invalid("database does not exist"));
    }
    let _lease = if options.apply {
        let lease = lease_file(path)?;
        lease.try_lock().map_err(|error| {
            invalid(format!(
                "database is in use; stop all evorch processes: {error}"
            ))
        })?;
        lease
    } else {
        preview_lease(path)?
    };
    let flags = if options.apply {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let (conn, preview_files) = if options.apply {
        (Connection::open_with_flags(path, flags)?, None)
    } else {
        let (conn, files) = read_only_preview_connection(path)?;
        (conn, Some(files))
    };
    conn.busy_timeout(Duration::ZERO)?;
    if options.apply {
        conn.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
        conn.execute_batch("BEGIN EXCLUSIVE; COMMIT;")
            .map_err(|error| {
                invalid(format!(
                    "database is in use; exclusive SQLite lock refused: {error}"
                ))
            })?;
    }
    let before = fs::metadata(path)
        .map_err(|error| StorageError::Io(error.to_string()))?
        .len();
    let mut summary = preview(&conn)?;
    summary.dry_run = !options.apply;
    summary.database_bytes_before = before;
    summary.database_bytes_after = before;
    let logical_bytes = conn.pragma_query_value(None, "page_count", |row| row.get::<_, i64>(0))?
        as u64
        * conn.pragma_query_value(None, "page_size", |row| row.get::<_, i64>(0))? as u64;
    let space_basis = before
        .max(logical_bytes)
        .max(crate::db::file_sizes(path)?.total());
    summary.vacuum_space_budget_bytes = space_basis.saturating_mul(2);
    if !options.apply {
        preview_files
            .as_ref()
            .expect("dry-run file verification")
            .verify()?;
        return Ok(summary);
    }
    // Verify the existing accounting schema before any payload write. Do not
    // migrate an old DB implicitly during maintenance.
    let version: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version < 23 || version as usize > crate::migrations::MIGRATIONS.len() {
        return Err(invalid(
            "unsupported maintenance schema; use current evorch to migrate first",
        ));
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    // Rewrite WAL may temporarily hold up to an original DB; full vacuum may
    // need another two DB-sized files. Conservative budgets, not reservations.
    require_space(
        parent,
        space_basis.saturating_mul(if options.no_vacuum { 1 } else { 3 }),
    )?;
    if !options.no_backup {
        let backup = options.backup.clone().unwrap_or_else(|| {
            let ns = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let mut name = path.as_os_str().to_os_string();
            name.push(format!(".before-task-progress-{ns}.db"));
            name.into()
        });
        let backup_parent = backup
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        // Include source scratch too even on the same volume.
        require_space(backup_parent, space_basis.saturating_mul(4))?;
        let mut file_options = OpenOptions::new();
        file_options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            file_options.mode(0o600);
        }
        let reserved = file_options
            .open(&backup)
            .map_err(|error| StorageError::Io(error.to_string()))?;
        drop(reserved);
        conn.execute(
            "VACUUM INTO ?1",
            [backup
                .to_str()
                .ok_or_else(|| invalid("backup path must be UTF-8"))?],
        )?;
        let check = Connection::open_with_flags(&backup, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let integrity: String = check.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(invalid("backup integrity check failed; source unchanged"));
        }
        File::open(&backup)
            .and_then(|file| file.sync_all())
            .map_err(|error| StorageError::Io(error.to_string()))?;
        summary.backup = Some(backup);
    }
    let through: i64 = conn.query_row("SELECT COALESCE(MAX(id),0) FROM events", [], |row| {
        row.get(0)
    })?;
    let mut after = i64::MIN;
    loop {
        let (rewritten, last) = strip_batch_after(&conn, after, through, options.batch_size)?;
        summary.rewritten_rows += rewritten;
        if rewritten == 0 {
            break;
        }
        after = last;
    }
    if !options.no_vacuum {
        require_space(parent, space_basis.saturating_mul(2))?;
        conn.execute_batch("VACUUM;")?;
        summary.vacuum = true;
    }
    let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
    if busy != 0 {
        return Err(invalid(
            "rewrite committed but WAL truncate is busy; rerun after closing all readers",
        ));
    }
    summary.checkpoint_truncated = true;
    summary.database_bytes_after = fs::metadata(path)
        .map_err(|error| StorageError::Io(error.to_string()))?
        .len();
    Ok(summary)
}

fn preview(conn: &Connection) -> Result<StripSummary, StorageError> {
    Ok(conn.query_row(&format!("SELECT COUNT(*), COALESCE(SUM(OCTET_LENGTH(payload)),0), COALESCE(SUM(OCTET_LENGTH(json_quote(json_extract(payload, '{CURSOR}')))),0), COALESCE(SUM(OCTET_LENGTH(payload) - OCTET_LENGTH(json_remove(payload, '{CURSOR}'))),0) FROM events WHERE {FILTER}"), [], |row| Ok(StripSummary { target_rows: row.get::<_, i64>(0)? as u64, payload_bytes: row.get::<_, i64>(1)? as u64, resume_cursor_bytes: row.get::<_, i64>(2)? as u64, estimated_payload_reduction_bytes: row.get::<_, i64>(3)? as u64, ..Default::default() }))?)
}

#[cfg(test)]
pub(super) fn strip_batch(
    conn: &Connection,
    through: i64,
    limit: usize,
) -> Result<u64, StorageError> {
    strip_batch_after(conn, i64::MIN, through, limit).map(|(count, _)| count)
}

fn strip_batch_after(
    conn: &Connection,
    after: i64,
    through: i64,
    limit: usize,
) -> Result<(u64, i64), StorageError> {
    let tx = conn.unchecked_transaction()?;
    let mut stmt = tx.prepare(&format!("SELECT id, session_id, OCTET_LENGTH(payload) - OCTET_LENGTH(json_remove(payload, '{CURSOR}')) FROM events WHERE id > ?1 AND id <= ?2 AND {FILTER} ORDER BY id LIMIT ?3"))?;
    let rows = stmt
        .query_map(params![after, through, limit as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    for (id, session, reduction) in &rows {
        tx.execute(
            &format!("UPDATE events SET payload = json_remove(payload, '{CURSOR}') WHERE id = ?1"),
            [id],
        )?;
        if let Some(session) = session {
            tx.execute("UPDATE sessions SET total_event_bytes = MAX(0, total_event_bytes - ?1) WHERE id = ?2", params![reduction, session])?;
        }
    }
    tx.commit()?;
    Ok((rows.len() as u64, rows.last().map_or(after, |row| row.0)))
}

pub(super) fn require_space(path: &Path, required: u64) -> Result<(), StorageError> {
    #[cfg(unix)]
    {
        let info =
            rustix::fs::statvfs(path).map_err(|error| StorageError::Io(error.to_string()))?;
        let available = info.f_bavail.saturating_mul(info.f_frsize);
        if available < required {
            return Err(invalid(format!(
                "insufficient free space: need {required} bytes, available {available}; free-space checks cannot reserve space"
            )));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (path, required);
        Err(invalid(
            "free-space checking is not supported on this platform; refusing apply",
        ))
    }
}

#[derive(Debug, PartialEq, Eq)]
struct FileStamp {
    bytes: u64,
    modified: SystemTime,
}

pub(super) struct PreviewFiles {
    paths: [PathBuf; 3],
    stamps: [Option<FileStamp>; 3],
}

impl PreviewFiles {
    fn stamps(paths: &[PathBuf; 3]) -> Result<[Option<FileStamp>; 3], StorageError> {
        let mut stamps = [None, None, None];
        for (path, stamp) in paths.iter().zip(&mut stamps) {
            match fs::metadata(path) {
                Ok(meta) if meta.is_file() => {
                    *stamp = Some(FileStamp {
                        bytes: meta.len(),
                        modified: meta
                            .modified()
                            .map_err(|error| StorageError::Io(error.to_string()))?,
                    });
                }
                Ok(_) => return Err(invalid("database or sidecar is not a regular file")),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(StorageError::Io(error.to_string())),
            }
        }
        Ok(stamps)
    }

    pub(super) fn verify(&self) -> Result<(), StorageError> {
        if Self::stamps(&self.paths)? != self.stamps {
            return Err(invalid(
                "database files changed during offline preview; stop all evorch processes and retry",
            ));
        }
        Ok(())
    }
}

// The caller holds the offline lease. Never open a WAL-bearing source with
// mode=ro alone: SQLite may create/init WAL/SHM despite a read-only main DB.
pub(super) fn read_only_preview_connection(
    path: &Path,
) -> Result<(Connection, PreviewFiles), StorageError> {
    let absolute = path
        .canonicalize()
        .map_err(|error| StorageError::Io(error.to_string()))?;
    let paths = [
        absolute.clone(),
        crate::db::suffixed_path(&absolute, "-wal"),
        crate::db::suffixed_path(&absolute, "-shm"),
    ];
    let stamps = PreviewFiles::stamps(&paths)?;
    if stamps[1].as_ref().is_some_and(|stamp| stamp.bytes != 0) {
        return Err(invalid(
            "offline preview refuses a nonempty WAL without opening SQLite; start evorch once and exit cleanly to checkpoint, or run explicit --apply while offline to checkpoint",
        ));
    }
    let files = PreviewFiles { paths, stamps };
    let mut uri = String::from("file:");
    let text = absolute
        .to_str()
        .ok_or_else(|| invalid("database path must be UTF-8"))?;
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri.push_str("?mode=ro&immutable=1");
    let conn = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    conn.busy_timeout(Duration::ZERO)?;
    files.verify()?;
    Ok((conn, files))
}
