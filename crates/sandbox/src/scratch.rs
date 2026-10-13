//! Disk-backed temporary command directories.
//!
//! A [`ScratchRoot`] keeps one private directory per name under a fixed parent,
//! so a caller can give shells the same `TMPDIR` across sandboxes and restarts.
//! Sandboxes without a configured directory fall back to a private one of their own.
//!
//! Neither path scans or recursively deletes on the caller's thread. Removal
//! renames the tree aside, which frees its path at once, then a bounded queue
//! and one throttled worker delete it. Crashed processes / full queues leave
//! renamed trees for [`ScratchRoot::purge_trash`] or OS temporary-file cleanup.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, mpsc};
use std::time::Duration;

const TRASH_PREFIX: &str = ".trash-";

/// Private per-name scratch directories under one parent directory.
#[derive(Debug)]
pub struct ScratchRoot {
    parent: PathBuf,
    ephemeral: bool,
}

impl ScratchRoot {
    /// `/var/tmp/evorch-scratch-<uid>-<scope>`. Nothing is created before [`Self::prepare`].
    ///
    /// # Errors
    /// Rejects a `scope` that is not a short ASCII alphanumeric name.
    pub fn fixed(scope: &str) -> io::Result<Self> {
        if scope.is_empty()
            || scope.len() > 64
            || !scope.bytes().all(|byte| byte.is_ascii_alphanumeric())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "scratch scope must be 1-64 ASCII alphanumeric characters",
            ));
        }
        Ok(Self::at(PathBuf::from(format!(
            "/var/tmp/evorch-scratch-{}-{scope}",
            euid()
        ))))
    }

    /// A caller-chosen parent, such as a test's temporary directory.
    pub fn at(parent: PathBuf) -> Self {
        Self {
            parent,
            ephemeral: false,
        }
    }

    /// A new randomly named parent under `/var/tmp`, removed when this value drops.
    ///
    /// # Errors
    /// Propagates directory creation failures.
    pub fn ephemeral() -> io::Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix(&format!("evorch-scratch-{}-", euid()))
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in("/var/tmp")?;
        Ok(Self {
            parent: directory.keep(),
            ephemeral: true,
        })
    }

    pub fn parent(&self) -> &Path {
        &self.parent
    }

    /// The directory for `name`, whether or not it exists.
    ///
    /// # Errors
    /// Rejects names that are not one plain, visible path component.
    pub fn dir(&self, name: &str) -> io::Result<PathBuf> {
        if name.is_empty()
            || name.len() > 128
            || name.starts_with('.')
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "scratch directory names must be plain ASCII path components",
            ));
        }
        Ok(self.parent.join(name))
    }

    /// Create `name`'s directory, with the private HOME layout sandboxes mount.
    ///
    /// # Errors
    /// Fails when the parent is not a private directory owned by the current user.
    pub fn prepare(&self, name: &str) -> io::Result<PathBuf> {
        let dir = self.dir(name)?;
        private_dir(&self.parent)?;
        private_dir(&dir)?;
        ensure_layout(&dir)?;
        Ok(dir)
    }

    /// Free `name`'s path now and delete its contents in the background.
    /// Returns whether a directory existed.
    ///
    /// # Errors
    /// Fails when the parent is not a private directory owned by the current user.
    pub fn discard(&self, name: &str) -> io::Result<bool> {
        let dir = self.dir(name)?;
        if !self.owned_parent()? || dir.symlink_metadata().is_err() {
            return Ok(false);
        }
        let trash = tempfile::Builder::new()
            .prefix(TRASH_PREFIX)
            .tempdir_in(&self.parent)?
            .keep();
        match fs::rename(&dir, &trash) {
            Ok(()) => {
                queue_cleanup(trash);
                Ok(true)
            }
            Err(error) => {
                let _ = fs::remove_dir(&trash);
                if error.kind() == io::ErrorKind::NotFound {
                    Ok(false)
                } else {
                    Err(error)
                }
            }
        }
    }

    /// Names of the existing directories, excluding trees already discarded.
    ///
    /// # Errors
    /// Propagates directory read failures other than a missing parent.
    pub fn names(&self) -> io::Result<Vec<String>> {
        if !self.owned_parent()? {
            return Ok(Vec::new());
        }
        Ok(fs::read_dir(&self.parent)?
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| !name.starts_with(TRASH_PREFIX) && self.dir(name).is_ok())
            .collect())
    }

    /// Queue deletion of trees discarded by processes that exited before deleting them.
    ///
    /// # Errors
    /// Propagates directory read failures other than a missing parent.
    pub fn purge_trash(&self) -> io::Result<()> {
        if !self.owned_parent()? {
            return Ok(());
        }
        for entry in fs::read_dir(&self.parent)?.filter_map(Result::ok) {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(TRASH_PREFIX))
            {
                queue_cleanup(entry.path());
            }
        }
        Ok(())
    }

    fn owned_parent(&self) -> io::Result<bool> {
        match fs::symlink_metadata(&self.parent) {
            Ok(meta) if is_private(&meta) => Ok(true),
            Ok(_) => Err(not_private()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

impl Drop for ScratchRoot {
    fn drop(&mut self) {
        if self.ephemeral {
            queue_cleanup(self.parent.clone());
        }
    }
}

#[derive(Debug)]
pub(super) struct Scratch {
    path: PathBuf,
}

impl Scratch {
    pub(super) fn new() -> io::Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("evorch-scratch-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in("/var/tmp")?;
        ensure_layout(directory.path())?;
        let path = directory.keep();
        Ok(Self { path })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        queue_cleanup(self.path.clone());
    }
}

/// Keep HOME and Cargo's lock directory on disk.
pub(super) fn ensure_layout(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir.join("home/.cargo"))
}

fn private_dir(path: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    if is_private(&fs::symlink_metadata(path)?) {
        Ok(())
    } else {
        Err(not_private())
    }
}

fn is_private(meta: &fs::Metadata) -> bool {
    meta.is_dir() && meta.uid() == euid() && meta.mode() & 0o077 == 0
}

fn not_private() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "scratch directory must be private and owned by the current user",
    )
}

fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn queue_cleanup(path: PathBuf) {
    static CLEANUP: OnceLock<Option<mpsc::SyncSender<PathBuf>>> = OnceLock::new();
    let sender = CLEANUP.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel::<PathBuf>(64);
        std::thread::Builder::new()
            .name("evorch-scratch-cleanup".into())
            .spawn(move || {
                for path in receiver {
                    cleanup_tree(&path, Duration::from_millis(10));
                }
            })
            .ok()
            .map(|_| sender)
    });
    if let Some(sender) = sender {
        // Never block teardown behind a large old scratch tree.
        let _ = sender.try_send(path);
    }
}

fn cleanup_tree(root: &Path, delay: Duration) {
    use rustix::fs::{AtFlags, Dir, Mode, OFlags, open, openat, unlinkat};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let Ok(root_fd) = open(root, flags, Mode::empty()) else {
        return;
    };
    let Ok(entries) = Dir::new(root_fd) else {
        return;
    };
    // Operate relative to open directory handles. NOFOLLOW prevents even a
    // concurrently swapped symlink from redirecting cleanup outside the tree.
    // Dir streams entries; memory is bounded by depth, not total file count.
    let mut stack = vec![(std::ffi::CString::default(), entries)];
    while let Some((_, entries)) = stack.last_mut() {
        match entries.next() {
            Some(Ok(entry)) => {
                let name = entry.file_name();
                if name == c"." || name == c".." {
                    continue;
                }
                if let Ok(fd) = entries.fd() {
                    match openat(fd, name, flags, Mode::empty()) {
                        Ok(child) => {
                            if let Ok(children) = Dir::new(child) {
                                stack.push((name.to_owned(), children));
                            }
                        }
                        Err(_) => {
                            let _ = unlinkat(fd, name, AtFlags::empty());
                        }
                    }
                }
            }
            Some(Err(_)) => {}
            None => {
                if let Some((name, _)) = stack.pop() {
                    if let Some((_, parent)) = stack.last() {
                        if let Ok(fd) = parent.fd() {
                            let _ = unlinkat(fd, name.as_c_str(), AtFlags::REMOVEDIR);
                        }
                    } else {
                        let _ = fs::remove_dir(root);
                    }
                }
            }
        }
        std::thread::sleep(delay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_is_private_disk_backed_and_cleanup_never_follows_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let scratch = Scratch::new().unwrap();
        assert!(scratch.path().starts_with("/var/tmp"));
        assert_eq!(
            fs::metadata(scratch.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let outside = tempfile::tempdir().unwrap();
        let protected = outside.path().join("keep");
        fs::write(&protected, "keep").unwrap();
        fs::create_dir(scratch.path().join("nested")).unwrap();
        fs::write(scratch.path().join("nested/file"), "remove").unwrap();
        symlink(outside.path(), scratch.path().join("external")).unwrap();
        cleanup_tree(scratch.path(), Duration::ZERO);
        assert!(!scratch.path().exists());
        assert_eq!(fs::read_to_string(protected).unwrap(), "keep");
    }

    // Given: a fixed scope / When: deriving the parent / Then: it is per-user, per-scope and lazy
    #[test]
    fn fixed_parent_is_derived_from_uid_and_scope_without_creating_it() {
        let root = ScratchRoot::fixed("abc123").unwrap();
        assert_eq!(
            root.parent(),
            Path::new(&format!("/var/tmp/evorch-scratch-{}-abc123", euid()))
        );
        assert_ne!(root.parent(), ScratchRoot::fixed("other").unwrap().parent());
        for scope in ["", "../x", "a/b", "dot.ted"] {
            assert!(ScratchRoot::fixed(scope).is_err(), "{scope}");
        }
        for name in ["", ".hidden", "a/b", "..", "white space"] {
            assert!(root.dir(name).is_err(), "{name}");
        }
    }

    // Given: a prepared directory with files / When: discarding it / Then: the path is free
    // at once, a later prepare starts empty, and the parent stays private
    #[test]
    fn discard_frees_the_path_immediately_and_prepare_recreates_it_empty() {
        let base = tempfile::tempdir().unwrap();
        let root = ScratchRoot::at(base.path().join("scratch"));
        let dir = root.prepare("t-thread").unwrap();
        assert!(dir.join("home/.cargo").is_dir());
        assert_eq!(
            fs::metadata(root.parent()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::write(dir.join("file"), "old").unwrap();
        assert_eq!(root.names().unwrap(), vec!["t-thread".to_owned()]);
        assert!(root.discard("t-thread").unwrap());
        assert!(!dir.exists());
        assert!(root.names().unwrap().is_empty());
        assert!(!root.discard("t-thread").unwrap());
        let again = root.prepare("t-thread").unwrap();
        assert_eq!(again, dir);
        assert!(!again.join("file").exists());
    }

    // Given: a parent that is group-readable / When: preparing / Then: it fails closed
    #[test]
    fn non_private_parent_is_rejected() {
        let base = tempfile::tempdir().unwrap();
        let parent = base.path().join("scratch");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        let root = ScratchRoot::at(parent);
        assert!(root.prepare("t-thread").is_err());
        assert!(root.discard("t-thread").is_err());
        let link = base.path().join("link");
        std::os::unix::fs::symlink(base.path(), &link).unwrap();
        assert!(ScratchRoot::at(link).prepare("t-thread").is_err());
    }
}
