//! The shared `sidebar.json` as a locked read-modify-write document.
//!
//! The file is shared with desktops that still write it themselves. They take
//! an exclusive lock on `<file>.lock` for every save and merge what they find on
//! disk into their own state; a writer using this module takes **the same lock**,
//! re-reads the file while holding it, applies its edit to what it read and writes
//! the result atomically. Nothing is written from a stale copy, so another
//! writer's change is never overwritten, and nothing is written when the file
//! cannot be read (an unreadable file is not "no file").
//!
//! The file also carries fields this crate does not interpret (sent messages and
//! composer drafts of a desktop). They are kept byte-for-byte as values; only the
//! sidebar part is edited here.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{SidebarError, SidebarState};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Another process holds the lock; nothing was read or written.
    #[error("the shared sidebar is locked by another process")]
    Busy,
    #[error("shared sidebar I/O failed: {0}")]
    Io(String),
    /// The file exists but is not a valid sidebar; it is left as it is.
    #[error("shared sidebar is unreadable: {0}")]
    Unreadable(String),
    #[error(transparent)]
    Invalid(#[from] SidebarError),
}

/// The sidebar plus the fields of the file this crate does not interpret.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SidebarDocument {
    #[serde(flatten)]
    pub sidebar: SidebarState,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

impl SidebarDocument {
    /// Removes every thread `deleted` names, with its family state and the
    /// messages and drafts that hang on it, and moves the active thread off a
    /// removed one. A tombstone wins over anything a saved copy still holds.
    /// Returns how many threads were removed.
    pub fn prune_threads(&mut self, deleted: &dyn Fn(&str) -> bool) -> usize {
        let before = self.sidebar.threads.len();
        let active_project = self
            .sidebar
            .active_thread
            .as_ref()
            .filter(|active| deleted(&active.to_string()))
            .and_then(|active| {
                self.sidebar
                    .threads
                    .iter()
                    .find(|thread| &thread.id == active)
                    .map(|thread| thread.project_id.clone())
            });
        self.sidebar
            .threads
            .retain(|thread| !deleted(&thread.id.to_string()));
        self.sidebar
            .families
            .retain(|family| !deleted(&family.key.to_string()));
        for key in ["user_messages", "composer_drafts"] {
            if let Some(Value::Array(items)) = self.rest.get_mut(key) {
                items.retain(|item| {
                    !item
                        .get("thread_id")
                        .and_then(Value::as_str)
                        .is_some_and(deleted)
                });
            }
        }
        if self
            .sidebar
            .active_thread
            .as_ref()
            .is_some_and(|active| deleted(&active.to_string()))
        {
            self.sidebar.active_thread = active_project.and_then(|project| {
                self.sidebar
                    .threads
                    .iter()
                    .find(|thread| thread.project_id == project)
                    .map(|thread| thread.id.clone())
            });
        }
        before - self.sidebar.threads.len()
    }
}

/// Where the shared sidebar lives.
#[derive(Debug, Clone)]
pub struct SidebarStore {
    path: PathBuf,
}

/// An exclusive hold on `<file>.lock`, released on drop.
#[derive(Debug)]
pub struct SidebarLock(std::fs::File);

impl Drop for SidebarLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

impl SidebarStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Takes the lock without waiting.
    ///
    /// # Errors
    /// [`StoreError::Busy`] while another process holds it.
    pub fn lock(&self) -> Result<SidebarLock, StoreError> {
        let mut name = self
            .path
            .file_name()
            .ok_or_else(|| StoreError::Io("the sidebar path has no file name".into()))?
            .to_os_string();
        name.push(".lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.path.with_file_name(name))
            .map_err(|error| StoreError::Io(error.to_string()))?;
        match file.try_lock() {
            Ok(()) => Ok(SidebarLock(file)),
            Err(std::fs::TryLockError::WouldBlock) => Err(StoreError::Busy),
            Err(std::fs::TryLockError::Error(error)) => Err(StoreError::Io(error.to_string())),
        }
    }

    /// Reads the file under `lock`. A missing file is an empty sidebar.
    ///
    /// # Errors
    /// The file cannot be read or is not a valid sidebar.
    pub fn read(&self, _lock: &SidebarLock) -> Result<SidebarDocument, StoreError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SidebarDocument::default());
            }
            Err(error) => return Err(StoreError::Io(error.to_string())),
        };
        Self::parse(&bytes)
    }

    /// Reads the file without the lock (a query). The write is atomic, so a
    /// reader sees either the old or the new file.
    ///
    /// # Errors
    /// See [`Self::read`].
    pub fn read_unlocked(&self) -> Result<SidebarDocument, StoreError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Self::parse(&bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(SidebarDocument::default())
            }
            Err(error) => Err(StoreError::Io(error.to_string())),
        }
    }

    fn parse(bytes: &[u8]) -> Result<SidebarDocument, StoreError> {
        let mut document: SidebarDocument = serde_json::from_slice(bytes)
            .map_err(|error| StoreError::Unreadable(error.to_string()))?;
        crate::normalize_versions(&mut document.sidebar.threads);
        document.sidebar.refresh_derived();
        Ok(document)
    }

    /// Validates and writes `document` atomically (temporary file, `fsync`,
    /// rename). Requires the lock.
    ///
    /// # Errors
    /// The document is invalid, or the file could not be written; the previous
    /// file is then untouched.
    pub fn write(
        &self,
        _lock: &SidebarLock,
        document: &SidebarDocument,
    ) -> Result<Vec<u8>, StoreError> {
        document.sidebar.validate()?;
        let bytes = serde_json::to_vec_pretty(document)
            .map_err(|error| StoreError::Io(error.to_string()))?;
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let tmp = self.path.with_extension(format!(
            "{}.{}.tmp",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&tmp, &self.path)
        })();
        if let Err(error) = result {
            let _ = std::fs::remove_file(&tmp);
            return Err(StoreError::Io(error.to_string()));
        }
        Ok(bytes)
    }
}
