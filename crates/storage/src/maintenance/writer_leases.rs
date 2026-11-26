//! Writer authority is independent of the shared/exclusive maintenance protocol.
use super::{
    invalid,
    offline::{same_lease, sibling_path},
};
use crate::StorageError;
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};

/// Startup only: ten 10ms polls bound contention to 100ms, off the async executor.
pub const WRITER_LEASE_WAIT: Duration = Duration::from_millis(100);
const POLL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone)]
pub struct DbWriterLease {
    lock: Arc<File>,
    identity: Arc<OnceLock<fs::Metadata>>,
    // The writer keeps root authority through startup failure and final writer join.
    root: Option<Arc<(File, File, PathBuf)>>,
    path: PathBuf,
}
#[derive(Debug, Clone)]
pub struct WriterLeases {
    db: DbWriterLease,
    root: Option<Arc<(File, File, PathBuf)>>,
}
fn io(error: std::io::Error) -> StorageError {
    StorageError::Io(error.to_string())
}
fn open(path: &Path) -> Result<File, StorageError> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    options.open(path).map_err(io)
}
fn wait_lock(
    file: &File,
    mut remaining: Duration,
    mut wait: impl FnMut(Duration),
) -> Result<(), StorageError> {
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(fs::TryLockError::WouldBlock) if !remaining.is_zero() => {
                let step = POLL.min(remaining);
                wait(step);
                remaining = remaining.saturating_sub(step);
            }
            Err(error) => {
                return Err(invalid(format!(
                    "DB writer lease unavailable (bounded wait exhausted); stop the other core/legacy embedded writer: {error}"
                )));
            }
        }
    }
}
impl DbWriterLease {
    /// Direct writers cooperate in shared mode; a core excludes them.
    pub(crate) fn shared(path: &Path) -> Result<Self, StorageError> {
        let lock = open(&sibling_path(path, ".writer-lock")?)?;
        lock.try_lock_shared().map_err(|error| invalid(format!("DB writer lease unavailable; a core is running. Connect the GUI to that core instead of --legacy-embedded: {error}")))?;
        Self::from_lock(path, lock)
    }
    fn from_lock(path: &Path, lock: File) -> Result<Self, StorageError> {
        let identity = Arc::new(OnceLock::new());
        if let Ok(metadata) = fs::metadata(path) {
            let _ = identity.set(metadata);
        }
        let lease = Self {
            lock: Arc::new(lock),
            identity,
            root: None,
            path: sibling_path(path, "")?,
        };
        lease.validate(path)?;
        Ok(lease)
    }
    pub(crate) fn bind_identity(&self, path: &Path) -> Result<(), StorageError> {
        // Never open the SQLite file ourselves: closing another FD can release its POSIX locks.
        let _ = self.identity.set(fs::metadata(path).map_err(io)?);
        self.validate(path)
    }
    pub(crate) fn validate(&self, path: &Path) -> Result<(), StorageError> {
        if sibling_path(path, "")? != self.path {
            return Err(invalid("DB writer identity mismatch"));
        }
        if let Some(held) = self.identity.get() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let current = fs::metadata(path).map_err(io)?;
                if current.nlink() != 1 {
                    return Err(invalid(
                        "hard-linked database cannot establish unique writer lock identity",
                    ));
                }
                if held.dev() != current.dev() || held.ino() != current.ino() {
                    return Err(invalid("DB writer identity mismatch"));
                }
            }
        }
        same_lease(&self.lock, &sibling_path(path, ".writer-lock")?)
    }
}
impl WriterLeases {
    /// Always DB then root. A busy root is not a startup failure and is never retried.
    pub fn acquire(db: &Path, root: &Path, wait: Duration) -> Result<Self, StorageError> {
        Self::acquire_with_wait(db, root, wait, std::thread::sleep)
    }
    fn acquire_with_wait(
        db: &Path,
        root: &Path,
        wait: Duration,
        pause: impl FnMut(Duration),
    ) -> Result<Self, StorageError> {
        let lock = open(&sibling_path(db, ".writer-lock")?)?;
        wait_lock(&lock, wait, pause)?;
        let mut db = DbWriterLease::from_lock(db, lock)?;
        let root = (|| {
            fs::create_dir_all(root).map_err(io)?;
            let lock = open(&root.join(".ownership-writer-lock"))?;
            match lock.try_lock() {
                Ok(()) => Ok(Some(Arc::new((
                    lock,
                    File::open(root).map_err(io)?,
                    root.canonicalize().map_err(io)?,
                )))),
                Err(fs::TryLockError::WouldBlock) => Ok(None),
                Err(error) => Err(invalid(format!("root writer lease unavailable: {error}"))),
            }
        })();
        // An unusable root also disables retention, not ordinary core operation.
        let root = match root {
            Ok(root) => root,
            Err(error) => {
                tracing::warn!(%error, "root writer lease unavailable; retention disabled until restart");
                None
            }
        };
        if root.is_none() {
            tracing::warn!(
                "root writer lease unavailable; automatic archive/root cleanup disabled until restart"
            );
        }
        db.root = root.clone();
        Ok(Self { db, root })
    }
    pub fn has_root_writer(&self) -> bool {
        self.root.is_some()
    }
    pub fn db_lease(&self) -> DbWriterLease {
        self.db.clone()
    }
    pub fn authority(&self) -> Option<crate::thread_retention::AutoArchiveAuthority<'_>> {
        self.root
            .as_ref()
            .filter(|_| self.db.identity.get().is_some())
            .map(|_| crate::thread_retention::AutoArchiveAuthority::new(self))
    }
    pub(crate) fn validate_archive_identity(
        &self,
        db: &Path,
        root: &Path,
    ) -> Result<(), StorageError> {
        self.db.validate(db)?;
        let (lock, directory, path) = self
            .root
            .as_deref()
            .ok_or_else(|| invalid("root writer authority unavailable"))?;
        if root.canonicalize().map_err(io)? != *path {
            return Err(invalid("root writer identity mismatch"));
        }
        same_lease(directory, root)?;
        same_lease(lock, &root.join(".ownership-writer-lock"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Database, Storage, StorageConfig};
    fn config(dir: &Path, name: &str) -> StorageConfig {
        StorageConfig {
            db_path: dir.join(name),
            ..Default::default()
        }
    }
    #[test]
    fn exclusive_core_blocks_direct_writer_but_not_inspection() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), "events.db");
        drop(Storage::open(config.clone()).unwrap());
        let leases = WriterLeases::acquire(&config.db_path, dir.path(), Duration::ZERO).unwrap();
        let maintenance =
            crate::shared_with_wait(&config.db_path, dir.path(), Duration::ZERO).unwrap();
        let writer = Storage::open_with_writer_lease(
            config.clone(),
            maintenance.db_lease().unwrap(),
            leases.db_lease(),
        )
        .unwrap();
        assert!(Database::open_for_inspection(&config.db_path).is_ok());
        assert!(Storage::open(config.clone()).is_err());
        assert!(WriterLeases::acquire(&config.db_path, dir.path(), Duration::ZERO).is_err());
        drop(leases);
        // The writer keeps its exact DB lock even after bootstrap drops its copy.
        assert!(Storage::open(config.clone()).is_err());
        let other = WriterLeases::acquire(&dir.path().join("other.db"), dir.path(), Duration::ZERO)
            .unwrap();
        assert!(
            !other.has_root_writer(),
            "root lock must outlive the writer too"
        );
        drop(other);
        writer.close();
        assert!(
            WriterLeases::acquire(&dir.path().join("other.db"), dir.path(), Duration::ZERO)
                .unwrap()
                .has_root_writer()
        );
        assert!(Storage::open(config).is_ok());
    }
    #[test]
    fn root_contention_is_optional_and_never_upgraded() {
        let dir = tempfile::tempdir().unwrap();
        drop(Storage::open(config(dir.path(), "a.db")).unwrap());
        drop(Storage::open(config(dir.path(), "b.db")).unwrap());
        let a =
            WriterLeases::acquire(&dir.path().join("a.db"), dir.path(), Duration::ZERO).unwrap();
        let b =
            WriterLeases::acquire(&dir.path().join("b.db"), dir.path(), Duration::ZERO).unwrap();
        assert!(a.authority().is_some());
        assert!(b.authority().is_none());
        drop(a);
        assert!(b.authority().is_none());
        assert!(Storage::open(config(dir.path(), "b.db")).is_err());
        drop(b);
        assert!(
            WriterLeases::acquire(&dir.path().join("b.db"), dir.path(), Duration::ZERO)
                .unwrap()
                .authority()
                .is_some()
        );
    }
    #[test]
    fn bounded_wait_is_injected_and_partial_startup_releases_locks() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), "events.db");
        let mut direct = Some(Storage::open(config.clone()).unwrap());
        let mut waits = Vec::new();
        let leases = WriterLeases::acquire_with_wait(
            &config.db_path,
            dir.path(),
            WRITER_LEASE_WAIT,
            |duration| {
                waits.push(duration);
                direct.take().unwrap().close();
            },
        )
        .unwrap();
        assert_eq!(waits, vec![POLL]);
        let mut waited = Duration::ZERO;
        assert!(
            WriterLeases::acquire_with_wait(&config.db_path, dir.path(), WRITER_LEASE_WAIT, |d| {
                waited += d
            })
            .is_err()
        );
        assert_eq!(waited, WRITER_LEASE_WAIT);
        drop(leases);
        assert!(Storage::open(config).is_ok());
    }
    #[test]
    fn authority_rejects_other_paths_and_replaced_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("events.db");
        drop(Storage::open(config(dir.path(), "events.db")).unwrap());
        let leases = WriterLeases::acquire(&db, dir.path(), Duration::ZERO).unwrap();
        let authority = leases.authority().unwrap();
        assert!(authority.validate(&db, dir.path()).is_ok());
        let other = dir.path().join("other");
        fs::create_dir(&other).unwrap();
        assert!(authority.validate(&db, &other).is_err());
        let other_db = dir.path().join("other.db");
        File::create(&other_db).unwrap();
        assert!(authority.validate(&other_db, dir.path()).is_err());
        fs::rename(&db, dir.path().join("old.db")).unwrap();
        File::create(&db).unwrap();
        assert!(authority.validate(&db, dir.path()).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn hard_link_aliases_cannot_split_db_writer_lock_identity() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path(), "events.db");
        drop(Storage::open(config.clone()).unwrap());
        let alias = dir.path().join("alias.db");
        fs::hard_link(&config.db_path, &alias).unwrap();
        assert!(WriterLeases::acquire(&alias, dir.path(), Duration::ZERO).is_err());
        assert!(Storage::open(config).is_err());
    }
}
