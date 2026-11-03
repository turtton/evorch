//! Offline cursor-only rewrite. Never use Database::open here: it migrates.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

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

/// How often a blocked shared acquisition looks again.
const SHARED_POLL: Duration = Duration::from_millis(50);

/// Both locks of the startup exclusive phase: the database lease and the
/// ownership-root lease. Holding them proves that no other evorch process has the
/// database open and that none shares the ownership root (owners, scratch).
///
/// Acquisition order is fixed: database first, then root. The root file is
/// `<ownership_root>/.ownership-lock` and, like the database lease, is never unlinked.
#[derive(Debug)]
pub struct ExclusiveLeases {
    db: File,
    root: File,
}

/// Both locks held shared for the whole life of a running process.
#[derive(Debug)]
pub struct SharedLeases {
    db: File,
    root: File,
}

fn root_lease_file(ownership_root: &Path) -> Result<File, StorageError> {
    let io = |error: std::io::Error| StorageError::Io(error.to_string());
    fs::create_dir_all(ownership_root).map_err(io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(ownership_root, fs::Permissions::from_mode(0o700)).map_err(io)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(ownership_root.join(".ownership-lock"))
        .map_err(io)
}

fn would_block(error: &fs::TryLockError) -> bool {
    matches!(error, fs::TryLockError::WouldBlock)
}

fn lock_error(what: &str, error: fs::TryLockError) -> StorageError {
    invalid(format!("{what} lease is unavailable: {error}"))
}

/// Take both locks exclusively without waiting. `Ok(None)` means another process
/// holds one of them; nothing stays locked then (a refused root lock releases
/// the database lock again).
///
/// # Errors
/// Fails when a lease file cannot be created or locked for another reason.
pub fn try_exclusive(
    db_path: &Path,
    ownership_root: &Path,
) -> Result<Option<ExclusiveLeases>, StorageError> {
    let db = lease_file(db_path)?;
    match db.try_lock() {
        Ok(()) => {}
        Err(error) if would_block(&error) => return Ok(None),
        Err(error) => return Err(lock_error("database", error)),
    }
    // A failure below drops `db`, which releases its lock.
    let root = root_lease_file(ownership_root)?;
    match root.try_lock() {
        Ok(()) => Ok(Some(ExclusiveLeases { db, root })),
        Err(error) if would_block(&error) => Ok(None),
        Err(error) => Err(lock_error("ownership root", error)),
    }
}

fn lock_shared_within(file: &File, what: &str, max_wait: Duration) -> Result<(), StorageError> {
    let started = std::time::Instant::now();
    loop {
        match file.try_lock_shared() {
            Ok(()) => return Ok(()),
            Err(error) if would_block(&error) => {
                if started.elapsed() >= max_wait {
                    return Err(invalid(format!(
                        "{what} lease is held exclusively by another process (waited {max_wait:?})"
                    )));
                }
                // The holder is another process (offline maintenance or a startup
                // phase); it can only be polled, it publishes no signal.
                std::thread::sleep(SHARED_POLL.min(max_wait.saturating_sub(started.elapsed())));
            }
            Err(error) => return Err(lock_error(what, error)),
        }
    }
}

fn shared_pair(db: File, root: File, max_wait: Duration) -> Result<SharedLeases, StorageError> {
    // A failure of the root lock drops `db` and with it the database lock.
    lock_shared_within(&db, "database", max_wait)?;
    lock_shared_within(&root, "ownership root", max_wait)?;
    Ok(SharedLeases { db, root })
}

/// Take both locks shared, waiting up to `max_wait` for each. Nothing stays
/// locked on failure.
///
/// # Errors
/// Fails when a lock stays exclusive for `max_wait` or a lease file is unusable.
pub fn shared_with_wait(
    db_path: &Path,
    ownership_root: &Path,
    max_wait: Duration,
) -> Result<SharedLeases, StorageError> {
    let db = lease_file(db_path)?;
    let root = root_lease_file(ownership_root)?;
    shared_pair(db, root, max_wait)
}

impl ExclusiveLeases {
    /// Give up exclusivity and hold both locks shared for the rest of the process.
    ///
    /// A flock conversion is not atomic: the exclusive lock is removed first, so
    /// another process may take a lock in between. **Read any state that must not
    /// change (tombstones, the binding) again after this returns.**
    ///
    /// # Errors
    /// Fails when another process holds a lock exclusively for `max_wait`; then
    /// nothing stays locked.
    pub fn downgrade(self, max_wait: Duration) -> Result<SharedLeases, StorageError> {
        let Self { db, root } = self;
        // Release both before waiting so two starting processes cannot wait on each other.
        db.unlock()
            .and_then(|()| root.unlock())
            .map_err(|error| StorageError::Io(error.to_string()))?;
        shared_pair(db, root, max_wait)
    }
}

impl SharedLeases {
    /// A handle on the same database lock for `Storage::open_with_lease`; the
    /// lock lasts until every handle is dropped.
    ///
    /// # Errors
    /// Fails when the file handle cannot be duplicated.
    pub fn db_lease(&self) -> Result<File, StorageError> {
        self.db
            .try_clone()
            .map_err(|error| StorageError::Io(error.to_string()))
    }

    /// A handle on the same ownership-root lock, for components that outlive this value.
    ///
    /// # Errors
    /// Fails when the file handle cannot be duplicated.
    pub fn root_lease(&self) -> Result<File, StorageError> {
        self.root
            .try_clone()
            .map_err(|error| StorageError::Io(error.to_string()))
    }
}

/// The right to remove a reclaimed family's files: one process per database holds
/// `<db>.reclaim-lock` exclusively. `Ok(None)` when another process holds it. The
/// returned file keeps the lock until dropped; the path is never unlinked.
///
/// # Errors
/// Fails when the lock file cannot be created or locked for another reason.
pub fn reclaim_lease(db_path: &Path) -> Result<Option<File>, StorageError> {
    let path = sibling_path(db_path, ".reclaim-lock")?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|error| StorageError::Io(error.to_string()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(error) if would_block(&error) => Ok(None),
        Err(error) => Err(lock_error("reclaim", error)),
    }
}

pub(super) fn lease_path(path: &Path) -> Result<PathBuf, StorageError> {
    sibling_path(path, ".maintenance-lock")
}

/// `<canonical db path><suffix>`; the database need not exist yet.
fn sibling_path(path: &Path, suffix: &str) -> Result<PathBuf, StorageError> {
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
    name.push(suffix);
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

/// Turns `foreign_keys` on for a connection opened with raw `rusqlite` (not through
/// `Database::open`) and checks that it took effect. It must run before the first
/// transaction: the pragma does nothing inside one. Every maintenance writer calls it,
/// so a row of a table with a foreign key (`run_messages`, the reference tables) is
/// never deleted behind its children's back.
pub(super) fn enable_foreign_keys(conn: &Connection) -> Result<(), StorageError> {
    conn.pragma_update(None, "foreign_keys", true)?;
    let on: i64 = conn.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    if on != 1 {
        return Err(invalid("foreign_keys could not be enabled for maintenance"));
    }
    Ok(())
}

/// Fails if any foreign key is violated: maintenance ends with this check.
pub(super) fn require_no_foreign_key_violations(conn: &Connection) -> Result<(), StorageError> {
    let violated: Option<String> = conn
        .query_row(
            "SELECT \"table\" FROM pragma_foreign_key_check LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match violated {
        None => Ok(()),
        Some(table) => Err(invalid(format!(
            "foreign key check failed after maintenance (a row of {table} has no parent)"
        ))),
    }
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
        let conn = Connection::open_with_flags(path, flags)?;
        enable_foreign_keys(&conn)?;
        (conn, None)
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
            "unsupported maintenance schema; a database in the old conversation format needs the conversion command first (a normal start never converts it)",
        ));
    }
    super::compact::check_current_format(&conn)?;
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

#[cfg(test)]
mod lease_tests;
