//! Durable model-facing shell numbers, independent of executor/run lifetimes.
//! Never delete this state to recover an error: saved histories may name any
//! previously issued number. External deletion/replacement of live state is not
//! supported. Job retention and the internal UUID index are separate concerns.
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::ToolError;

const FORMAT: &str = "evorch-shell-handles-v1\n";
// History is untrusted input: it may advance the counter only within this
// range. Normal allocation retains u64 capacity; already-issued high numbers
// can still be restored with a no-op reservation.
pub(crate) const MAX_RESTORED_HANDLE_FLOOR: u64 = u32::MAX as u64;

#[derive(Default)]
pub(crate) struct HandleAllocator {
    // Only unit tests override the directory; production resolves user state.
    #[cfg(test)]
    root: Option<PathBuf>,
}

impl HandleAllocator {
    #[cfg(test)]
    pub(crate) fn for_test(root: PathBuf) -> Self {
        Self { root: Some(root) }
    }

    fn root(&self) -> io::Result<PathBuf> {
        #[cfg(test)]
        {
            static STATE: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
            Ok(self.root.clone().unwrap_or_else(|| {
                STATE
                    .get_or_init(|| tempfile::tempdir().expect("test shell handle directory"))
                    .path()
                    .join("shell-job-handles")
            }))
        }
        #[cfg(not(test))]
        config::user_config_dir()
            .map(|root| root.join("shell-job-handles"))
            .ok_or_else(|| io::Error::other("shell handle state directory is unavailable"))
    }

    pub(crate) fn allocate(&self) -> Result<u64, ToolError> {
        self.update(None)
    }

    pub(crate) fn reserve(&self, next: u64) -> Result<(), ToolError> {
        self.update(Some(next)).map(|_| ())
    }

    fn update(&self, reserve: Option<u64>) -> Result<u64, ToolError> {
        self.try_update(reserve).map_err(|error| ToolError::Io {
            detail: format!(
                "durable shell job handle allocation failed: {error}; state must not be reset"
            ),
        })
    }

    fn try_update(&self, reserve: Option<u64>) -> io::Result<u64> {
        let root = self.root()?;
        // A large reservation needs existing state to prove it is a no-op.
        // Do not even initialize a missing directory for rejected history.
        if reserve.is_none_or(|next| next <= MAX_RESTORED_HANDLE_FLOOR) {
            initialize(&root)?;
        }
        // Never create a missing lock in an existing directory, or replace it.
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join("lock"))?;
        lock.lock()?;
        let counter = root.join("counter");
        let mut text = String::new();
        File::open(&counter)?.take(128).read_to_string(&mut text)?;
        let number = text
            .strip_prefix(FORMAT)
            .and_then(|text| text.strip_suffix('\n'))
            .filter(|text| {
                !text.is_empty() && text.len() <= 20 && text.bytes().all(|b| b.is_ascii_digit())
            })
            .and_then(|text| text.parse::<u64>().ok())
            .ok_or_else(|| io::Error::other("shell handle counter is corrupt"))?;
        let next = match reserve {
            Some(minimum) if minimum > number && minimum > MAX_RESTORED_HANDLE_FLOOR => {
                return Err(io::Error::other(format!(
                    "restored shell handle floor {minimum} exceeds safe reservation limit {MAX_RESTORED_HANDLE_FLOOR}"
                )));
            }
            Some(minimum) => number.max(minimum),
            None => number
                .checked_add(1)
                .ok_or_else(|| io::Error::other("shell job handle capacity exhausted"))?,
        };
        if next != number {
            let mut temp = tempfile::NamedTempFile::new_in(&root)?;
            writeln!(temp, "{FORMAT}{next}")?;
            temp.as_file().sync_all()?;
            temp.persist(&counter).map_err(|error| error.error)?;
        }
        // Also sync no-op reservations: a concurrent initializer may just have
        // published the directory, or a previous sync may have failed.
        File::open(&counter)?.sync_all()?;
        sync_ancestors(&root)?;
        Ok(number)
    }
}

fn initialize(root: &Path) -> io::Result<()> {
    match root.symlink_metadata() {
        Ok(meta) if meta.is_dir() => return Ok(()),
        Ok(_) => return Err(io::Error::other("shell handle state is not a directory")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let parent = root
        .parent()
        .ok_or_else(|| io::Error::other("state parent missing"))?;
    std::fs::create_dir_all(parent)?;
    // Prepare BOTH files before atomically publishing the directory. No other
    // process can see a lock-only first-run state or overwrite an issued value.
    let temp = tempfile::tempdir_in(parent)?;
    File::create(temp.path().join("lock"))?.sync_all()?;
    let mut counter = File::create(temp.path().join("counter"))?;
    writeln!(counter, "{FORMAT}0")?;
    counter.sync_all()?;
    File::open(temp.path())?.sync_all()?;
    publish(temp.path(), root)?;
    sync_ancestors(parent)
}

fn sync_ancestors(path: &Path) -> io::Result<()> {
    // create_dir_all may have introduced user config directories too. Persist
    // every directory entry up to the filesystem root before issuing a number.
    for directory in path.ancestors().filter(|path| !path.as_os_str().is_empty()) {
        File::open(directory)?.sync_all()?;
    }
    Ok(())
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "redox"
))]
fn publish(from: &Path, to: &Path) -> io::Result<()> {
    match rustix::fs::renameat_with(
        rustix::fs::CWD,
        from,
        rustix::fs::CWD,
        to,
        rustix::fs::RenameFlags::NOREPLACE,
    ) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::EXIST) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "redox"
)))]
fn publish(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::other(
        "atomic shell handle state initialization is unsupported on this platform",
    ))
}

#[cfg(test)]
mod tests;
