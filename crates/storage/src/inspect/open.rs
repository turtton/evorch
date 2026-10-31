//! Opening a database for inspection without writing to it.
//!
//! A plain read-only open of a WAL database can create the `-wal` and `-shm` files,
//! and a database opened `immutable` is only safe while nothing writes it. Neither
//! can be decided by looking at the files once and then opening: a writer may exit or
//! start in between. So the open takes locks first and decides under them:
//!
//! 1. A shared `fcntl` lock on the whole database file, the lock a SQLite reader
//!    holds. A WAL connection that closes last must take the file exclusively before it
//!    checkpoints and deletes the sidecars, so while the lock is held no exiting writer
//!    can remove them between the check and the open.
//! 2. The maintenance lease, which every current writer holds shared for its whole
//!    life and which offline maintenance holds exclusively.
//!
//! With a `-wal` and its `-shm` the database is opened with a read-only wal-index, so
//! SQLite reads the WAL without writing either file; the lease is held shared. With no
//! WAL to read the database is opened `immutable`, which is only sound while nothing
//! writes it, so the lease is taken exclusively: no current evorch can start (it fails
//! fast with "under offline maintenance") until the inspection ends, and the files'
//! identity is compared at the end to catch a writer that does not honour the lease.
//! Every other state is refused without touching anything.
//!
//! The one file this may create is the empty lease file itself, which is what makes
//! the exclusion possible.

use std::fs::{File, OpenOptions};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rusqlite::{Connection, OpenFlags};

use crate::{Database, StorageError};

/// How a database was opened for inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectionMode {
    /// The database has a WAL with its index; both are read without being written.
    LiveWal,
    /// No WAL content exists and no writer may start: the database file is read alone.
    NoWal,
}

/// A database opened by [`Database::open_for_inspection`], with the locks that keep it
/// stable held until it is dropped.
#[derive(Debug)]
pub struct InspectedDatabase {
    database: Database,
    guard: Guard,
}

impl Deref for InspectedDatabase {
    type Target = Database;

    fn deref(&self) -> &Database {
        &self.database
    }
}

impl InspectedDatabase {
    /// How the database was opened.
    pub fn mode(&self) -> InspectionMode {
        self.guard.mode
    }

    /// Confirms the files were not changed while they were read. Call it before using
    /// anything read from the database, and discard the results if it fails.
    ///
    /// # Errors
    /// Fails if a database opened without a WAL changed under the reader.
    pub fn finish(&self) -> Result<(), StorageError> {
        let Some(before) = &self.guard.stamps else {
            return Ok(());
        };
        if &Stamps::of(&self.guard.paths)? == before {
            Ok(())
        } else {
            Err(refuse(
                "the database files changed during inspection by a writer that does not use the maintenance lease; nothing was reported, retry once it has stopped",
            ))
        }
    }
}

/// Kept after the connection in [`InspectedDatabase`], so the locks outlive it.
#[derive(Debug)]
struct Guard {
    mode: InspectionMode,
    /// Files compared by [`InspectedDatabase::finish`] when only the database is read.
    stamps: Option<Stamps>,
    paths: [PathBuf; 3],
    _lease: File,
    _pin: File,
}

#[derive(Debug, PartialEq, Eq)]
struct Stamps([Option<Stamp>; 3]);

#[derive(Debug, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    inode: u64,
}

impl Stamps {
    fn of(paths: &[PathBuf; 3]) -> Result<Self, StorageError> {
        let stamp = |path: &PathBuf| -> Result<Option<Stamp>, StorageError> {
            match std::fs::metadata(path) {
                Ok(metadata) => Ok(Some(Stamp {
                    len: metadata.len(),
                    modified: metadata.modified().ok(),
                    #[cfg(unix)]
                    inode: std::os::unix::fs::MetadataExt::ino(&metadata),
                })),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(StorageError::Io(error.to_string())),
            }
        };
        Ok(Self([
            stamp(&paths[0])?,
            stamp(&paths[1])?,
            stamp(&paths[2])?,
        ]))
    }
}

fn refuse(message: &str) -> StorageError {
    StorageError::Serialization(message.into())
}

impl Database {
    /// Like [`Database::open_read_only`], but never creates, resizes or writes the
    /// database, its `-wal` or its `-shm`, and still reads what the WAL holds. See the
    /// module documentation for how.
    ///
    /// # Errors
    /// Fails like [`Database::open_read_only`]. It also refuses, changing nothing, when
    /// reading could not be done without writing: a WAL with data but no `-shm`, a
    /// database another process holds exclusively or is opening or closing, and a
    /// database under offline maintenance.
    pub fn open_for_inspection(path: &Path) -> Result<InspectedDatabase, StorageError> {
        let absolute = path
            .canonicalize()
            .map_err(|error| StorageError::Io(error.to_string()))?;
        let pin = pin(&absolute)?;
        let paths = [
            absolute.clone(),
            crate::db::suffixed_path(&absolute, "-wal"),
            crate::db::suffixed_path(&absolute, "-shm"),
        ];
        let before = Stamps::of(&paths)?;
        let wal_bytes = before.0[1].as_ref().map(|stamp| stamp.len);
        let (mode, options, lease) = match (wal_bytes, before.0[2].is_some()) {
            (Some(_), true) => {
                let lease = lease_file(&absolute)?;
                lease.try_lock_shared().map_err(|error| {
                    refuse(&format!("database is under offline maintenance: {error}"))
                })?;
                (InspectionMode::LiveWal, "mode=ro&readonly_shm=1", lease)
            }
            (Some(bytes), false) if bytes > 0 => {
                return Err(refuse(
                    "the write-ahead log holds data but its -shm index is missing; reading it would create the index, so start evorch once and exit cleanly to checkpoint",
                ));
            }
            _ => {
                let lease = lease_file(&absolute)?;
                lease.try_lock().map_err(|error| {
                    refuse(&format!(
                        "database is in use, or being opened or closed, by another evorch process or under offline maintenance; retry shortly: {error}"
                    ))
                })?;
                (InspectionMode::NoWal, "mode=ro&immutable=1", lease)
            }
        };
        let text = absolute
            .to_str()
            .ok_or_else(|| StorageError::Io("database path must be UTF-8".into()))?;
        let mut uri = String::from("file:");
        for byte in text.bytes() {
            if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
                uri.push(char::from(byte));
            } else {
                uri.push_str(&format!("%{byte:02X}"));
            }
        }
        uri.push('?');
        uri.push_str(options);
        let conn = Connection::open_with_flags(
            uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let database = Self::checked(conn)?;
        Ok(InspectedDatabase {
            database,
            guard: Guard {
                mode,
                stamps: (mode == InspectionMode::NoWal).then_some(before),
                paths,
                _lease: lease,
                _pin: pin,
            },
        })
    }
}

/// A shared lock on the whole database file, as a SQLite reader holds. It must outlive
/// the connection: POSIX locks are per process and are all dropped when any descriptor
/// of the file closes, so nothing else in the process may open and close the file.
fn pin(database: &Path) -> Result<File, StorageError> {
    let file = File::open(database).map_err(|error| StorageError::Io(error.to_string()))?;
    #[cfg(unix)]
    rustix::fs::fcntl_lock(&file, rustix::fs::FlockOperation::NonBlockingLockShared).map_err(
        |error| {
            refuse(&format!(
                "database is locked by another process, possibly offline maintenance: {error}"
            ))
        },
    )?;
    Ok(file)
}

/// The maintenance lease writers and offline maintenance share, at the path
/// `maintenance::offline` uses.
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
