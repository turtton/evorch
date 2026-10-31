//! Opening a database for inspection: a plain SQLite reader that cannot merge the WAL.
//!
//! The connection is read-only and follows SQLite's own reader protocol, like any other
//! reader of a live database, so SQLite may create and use the `-wal` and `-shm`
//! files. What inspection guarantees is that it never changes the database file: a
//! read-only connection cannot write it, and checkpointing when the connection closes
//! is disabled as well, so closing the last connection cannot merge a WAL into it.
//! Consistency comes from the callers reading inside one transaction.
//!
//! No file of the database is opened by anything but SQLite. POSIX locks belong to the
//! process and are all dropped when any descriptor of the file closes, so a descriptor
//! of our own would release the locks of every other connection to it in this process.
//!
//! Offline maintenance rewrites the file and holds the maintenance lease exclusively. The
//! lease is a separate file, so locking it cannot interfere with SQLite's locks on the
//! database. Inspection holds it shared, which writers do too, so it never keeps an
//! evorch from starting; it is refused if maintenance holds the lease, and maintenance
//! cannot start while an inspection holds it.

use std::fs::{File, OpenOptions};
use std::ops::Deref;
use std::path::{Path, PathBuf};

use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags};

use crate::{Database, StorageError};

/// A database opened by [`Database::open_for_inspection`]. The shared maintenance lease
/// is held until it is dropped.
#[derive(Debug)]
pub struct InspectedDatabase {
    database: Database,
    _lease: File,
}

impl Deref for InspectedDatabase {
    type Target = Database;

    fn deref(&self) -> &Database {
        &self.database
    }
}

impl Database {
    /// Opens an existing database read-only, without migrating it and without ever
    /// writing the database file or checkpointing its WAL. See the module documentation.
    ///
    /// # Errors
    /// Fails like [`Database::open_read_only`], and when offline maintenance holds the
    /// maintenance lease.
    pub fn open_for_inspection(path: &Path) -> Result<InspectedDatabase, StorageError> {
        let absolute = path
            .canonicalize()
            .map_err(|error| StorageError::Io(error.to_string()))?;
        let lease = lease_file(&absolute)?;
        lease.try_lock_shared().map_err(|error| {
            StorageError::Serialization(format!("database is under offline maintenance: {error}"))
        })?;
        let conn = Connection::open_with_flags(
            &absolute,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
        Ok(InspectedDatabase {
            database: Self::checked(conn)?,
            _lease: lease,
        })
    }
}

/// The maintenance lease writers and offline maintenance share, at the path
/// `maintenance::offline` uses; created if it does not exist yet, as a starting writer
/// would.
fn lease_file(database: &Path) -> Result<File, StorageError> {
    let mut name = database.as_os_str().to_owned();
    name.push(".maintenance-lock");
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(PathBuf::from(name))
        .map_err(|error| StorageError::Io(error.to_string()))
}

#[cfg(test)]
mod tests;
