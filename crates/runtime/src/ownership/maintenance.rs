//! Explicit offline maintenance; never invoked by owner startup.
use std::path::Path;

use rusqlite::{Connection, OpenFlags};

/// Remove legacy, unredacted transcripts and reclaim their pages.
///
/// The operator must stop **all** evorch processes first. Socket files (even
/// stale ones) cause refusal, as does any conflicting SQLite access. EXCLUSIVE
/// locking mode retains the database lock across the UPDATE commit and VACUUM;
/// a process starting concurrently cannot access a partially cleaned database.
/// No schema is created, and this function is never called on startup.
pub fn compact_owner_checkpoints(
    root: &Path,
    all_owners_stopped: bool,
) -> Result<usize, super::RegistryError> {
    let refuse = |reason: &str| {
        super::RegistryError::Preparation(format!("offline owner maintenance refused: {reason}"))
    };
    if !all_owners_stopped {
        return Err(refuse(
            "stop all evorch processes and acknowledge --all-owners-stopped",
        ));
    }
    let check_sockets = || -> Result<(), super::RegistryError> {
        for entry in std::fs::read_dir(root)? {
            if entry?
                .path()
                .extension()
                .is_some_and(|extension| extension == "sock")
            {
                return Err(refuse(
                    "owner socket exists; stop owners (remove stale sockets only after confirming they are stopped)",
                ));
            }
        }
        Ok(())
    };
    check_sockets()?;
    let connection = Connection::open_with_flags(
        root.join("owners.db"),
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(std::time::Duration::ZERO)?;
    let journal: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    if journal != "delete" {
        return Err(refuse("expected rollback DELETE journal mode"));
    }
    connection
        .execute_batch("PRAGMA locking_mode=EXCLUSIVE; PRAGMA secure_delete=ON; BEGIN EXCLUSIVE")?;
    check_sockets()?;
    let changed = connection.execute("UPDATE owner_checkpoints SET payload='[]'", [])?;
    connection.execute_batch("COMMIT; VACUUM")?;
    // SQLite keeps the exclusive lock until this connection closes, including
    // while VACUUM rebuilds the file. A failed VACUUM can safely be rerun offline.
    Ok(changed)
}
