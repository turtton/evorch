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
/// Records which ownership root a fixed parent belongs to. Dot-prefixed, so
/// [`ScratchRoot::dir`] never names it and sweeps never treat it as a directory.
const IDENTITY_MARKER: &str = ".owner";

/// What [`ScratchRoot::remove_bounded`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundedRemoval {
    /// Entries actually unlinked by this call.
    pub removed_entries: usize,
    /// The directory is gone from disk.
    pub complete: bool,
}

fn with_path(error: io::Error, path: &Path) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

/// Unlink one entry, counting it. An entry that vanished meanwhile is not an error
/// and not counted; any other failure is.
fn unlink(
    path: &Path,
    remove: impl FnOnce(&Path) -> io::Result<()>,
    progress: &mut BoundedRemoval,
) -> io::Result<()> {
    match remove(path) {
        Ok(()) => {
            progress.removed_entries += 1;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(with_path(error, path)),
    }
}

/// A failed [`ScratchRoot::remove_bounded`]: what it had unlinked when it stopped.
#[derive(Debug)]
pub struct BoundedError {
    /// Entries unlinked before the failure; they are gone and count against a budget.
    pub removed_entries: usize,
    pub source: io::Error,
}

impl std::fmt::Display for BoundedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} (after removing {} entries)",
            self.source, self.removed_entries
        )
    }
}

impl std::error::Error for BoundedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Private per-name scratch directories under one parent directory.
#[derive(Debug)]
pub struct ScratchRoot {
    parent: PathBuf,
    ephemeral: bool,
    /// The full identity a fixed parent's marker must hold. The parent name carries
    /// only a short prefix of it, so two identities can share a parent.
    identity: Option<String>,
}

impl ScratchRoot {
    /// `/var/tmp/evo-<uid>-<scope>`. Nothing is created before [`Self::prepare`].
    ///
    /// `scope` is a short, path-safe prefix of `identity`. The first `prepare` writes
    /// `identity` into the parent; every later use fails closed when the parent holds a
    /// different one, so two identities whose scopes collide never share a parent.
    ///
    /// # Errors
    /// Rejects a `scope` that is not a short ASCII alphanumeric name.
    pub fn fixed(scope: &str, identity: &str) -> io::Result<Self> {
        if scope.is_empty()
            || scope.len() > 64
            || !scope.bytes().all(|byte| byte.is_ascii_alphanumeric())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "scratch scope must be 1-64 ASCII alphanumeric characters",
            ));
        }
        Ok(
            Self::at(PathBuf::from(format!("/var/tmp/evo-{}-{scope}", euid())))
                .identified(identity),
        )
    }

    fn identified(mut self, identity: &str) -> Self {
        self.identity = Some(identity.to_owned());
        self
    }

    /// A caller-chosen parent, such as a test's temporary directory.
    pub fn at(parent: PathBuf) -> Self {
        Self {
            parent,
            ephemeral: false,
            identity: None,
        }
    }

    /// A new randomly named parent under `/var/tmp`, removed when this value drops.
    ///
    /// # Errors
    /// Propagates directory creation failures.
    pub fn ephemeral() -> io::Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix(&format!("evo-{}-", euid()))
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in("/var/tmp")?;
        Ok(Self {
            parent: directory.keep(),
            ephemeral: true,
            identity: None,
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
        self.check_identity(true)?;
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

    /// Delete `name`'s tree in place, entry by entry, without following links, and
    /// report what was actually removed. At most `max_entries` entries (files,
    /// links, directories) are unlinked; a tree that does not fit stays on disk, partly
    /// emptied, and the next call continues with what is left. `complete` is true only
    /// after the directory itself was unlinked (or never existed).
    ///
    /// Unlike [`Self::discard`], nothing is handed to a background worker: the answer
    /// is the state of the disk. Use it where a caller must know a deletion happened.
    ///
    /// # Errors
    /// Fails when the parent is not a private directory of this user, or on the first
    /// unlink / read error other than "not found" (the path is in the message). The
    /// entries removed before it stay removed, and [`BoundedError::removed_entries`]
    /// says how many, so the caller can charge them to its budget.
    pub fn remove_bounded(
        &self,
        name: &str,
        max_entries: usize,
    ) -> Result<BoundedRemoval, BoundedError> {
        let mut progress = BoundedRemoval {
            removed_entries: 0,
            complete: true,
        };
        match self.remove_bounded_into(name, max_entries, &mut progress) {
            Ok(()) => Ok(progress),
            Err(source) => Err(BoundedError {
                removed_entries: progress.removed_entries,
                source,
            }),
        }
    }

    fn remove_bounded_into(
        &self,
        name: &str,
        max_entries: usize,
        progress: &mut BoundedRemoval,
    ) -> io::Result<()> {
        let dir = self.dir(name)?;
        if !self.owned_parent()? {
            return Ok(());
        }
        match dir.symlink_metadata() {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(with_path(error, &dir)),
            // A link or file where the directory belongs: remove that entry itself.
            Ok(meta) if !meta.is_dir() => {
                if max_entries == 0 {
                    progress.complete = false;
                    return Ok(());
                }
                unlink(&dir, |path| fs::remove_file(path), progress)?;
                return Ok(());
            }
            Ok(_) => {}
        }
        let mut stack = vec![(
            dir.clone(),
            fs::read_dir(&dir).map_err(|e| with_path(e, &dir))?,
        )];
        while let Some((path, entries)) = stack.last_mut() {
            match entries.next() {
                Some(Ok(entry)) => {
                    if progress.removed_entries >= max_entries {
                        progress.complete = false;
                        return Ok(());
                    }
                    let child = entry.path();
                    // `file_type` does not follow links: a link is removed, never entered.
                    let kind = entry.file_type().map_err(|e| with_path(e, &child))?;
                    if kind.is_dir() {
                        let reader = fs::read_dir(&child).map_err(|e| with_path(e, &child))?;
                        stack.push((child, reader));
                    } else {
                        unlink(&child, |path| fs::remove_file(path), progress)?;
                    }
                }
                Some(Err(error)) => return Err(with_path(error, path)),
                None => {
                    if progress.removed_entries >= max_entries {
                        progress.complete = false;
                        return Ok(());
                    }
                    let path = path.clone();
                    stack.pop();
                    unlink(&path, |path| fs::remove_dir(path), progress)?;
                }
            }
        }
        Ok(())
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
            Ok(meta) if is_private(&meta) => self.check_identity(false),
            Ok(_) => Err(not_private()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

impl ScratchRoot {
    /// Whether the parent is this root's, failing when it is another identity's.
    /// `create` claims a parent without a marker (atomically, so concurrent first users
    /// agree); otherwise such a parent is not this root's yet and reads as absent. A
    /// directory only appears under a parent after its marker, so an unmarked parent
    /// holds nothing, and a colliding root can never delete what its owner is about to
    /// create. The parent is private to this user, so only this user's processes write
    /// the marker.
    fn check_identity(&self, create: bool) -> io::Result<bool> {
        let Some(identity) = &self.identity else {
            return Ok(true);
        };
        let marker = self.parent.join(IDENTITY_MARKER);
        let matches = |found: &str| -> io::Result<bool> {
            if found == identity {
                Ok(true)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "{} belongs to another ownership root (scratch scope collision)",
                        self.parent.display()
                    ),
                ))
            }
        };
        match read_marker(&marker) {
            Ok(found) => return matches(&found),
            Err(error) if error.kind() == io::ErrorKind::NotFound && create => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(with_path(error, &marker)),
        }
        let mut staged = tempfile::NamedTempFile::new_in(&self.parent)?;
        io::Write::write_all(&mut staged, identity.as_bytes())?;
        match staged.persist_noclobber(&marker) {
            Ok(_) => Ok(true),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                matches(&read_marker(&marker).map_err(|error| with_path(error, &marker))?)
            }
            Err(error) => Err(with_path(error.error, &marker)),
        }
    }
}

fn read_marker(path: &Path) -> io::Result<String> {
    use rustix::fs::{Mode, OFlags, open};
    let fd = open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    io::read_to_string(io::Read::take(fs::File::from(fd), 4096))
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
            .prefix("evo-")
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
    use rustix::fs::{Mode, OFlags, mkdirat, open, openat};
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut fd = open(dir, flags, Mode::empty())?;
    // Shells can replace HOME entries between commands. Never let layout
    // preparation create host directories through such a link.
    for name in ["home", ".cargo"] {
        match mkdirat(&fd, name, Mode::RWXU) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(e) => return Err(e.into()),
        }
        fd = openat(&fd, name, flags, Mode::empty())?;
    }
    Ok(())
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
        let root = ScratchRoot::fixed("abc123", "abc123full").unwrap();
        assert_eq!(
            root.parent(),
            Path::new(&format!("/var/tmp/evo-{}-abc123", euid()))
        );
        assert_ne!(
            root.parent(),
            ScratchRoot::fixed("other", "otherfull").unwrap().parent()
        );
        for scope in ["", "../x", "a/b", "dot.ted"] {
            assert!(ScratchRoot::fixed(scope, "id").is_err(), "{scope}");
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

    // Given: two identities whose scopes (and so parents) collide / When: the second uses
    // the parent / Then: it fails closed and never touches the first one's directories;
    // the same identity keeps sharing, and the marker is not a directory name
    #[test]
    fn parent_shared_by_colliding_identities_is_refused() {
        let base = tempfile::tempdir().unwrap();
        let parent = base.path().join("scratch");
        let root = |identity: &str| ScratchRoot::at(parent.clone()).identified(identity);
        let (first, same, other) = (root("first"), root("first"), root("other"));
        assert!(first.names().unwrap().is_empty(), "no parent yet is fine");
        let dir = first.prepare("thread").unwrap();
        fs::write(dir.join("file"), "keep").unwrap();
        assert_eq!(same.prepare("thread").unwrap(), dir);
        assert_eq!(same.names().unwrap(), vec!["thread".to_owned()]);

        assert!(other.prepare("thread").is_err());
        assert!(other.prepare("elsewhere").is_err());
        assert!(other.names().is_err());
        assert!(other.discard("thread").is_err());
        assert!(other.remove_bounded("thread", usize::MAX).is_err());
        assert!(other.purge_trash().is_err());
        assert_eq!(fs::read_to_string(dir.join("file")).unwrap(), "keep");
        assert!(!parent.join("elsewhere").exists());

        let marker = parent.join(IDENTITY_MARKER);
        fs::remove_file(&marker).unwrap();
        std::os::unix::fs::symlink(base.path(), &marker).unwrap();
        assert!(
            first.prepare("thread").is_err(),
            "a linked marker is refused"
        );
    }

    // Given: root A created the parent but has not published its marker yet / When: a
    // colliding root B cleans up, then A finishes preparing / Then: B changes nothing
    // while the parent is unmarked, and is refused once A owns it, so A's directory
    // survives every interleaving
    #[test]
    fn cleanup_by_a_colliding_root_cannot_precede_the_owners_marker() {
        let base = tempfile::tempdir().unwrap();
        let parent = base.path().join("scratch");
        let root = |identity: &str| ScratchRoot::at(parent.clone()).identified(identity);
        let (a, b) = (root("a"), root("b"));

        private_dir(&parent).unwrap();
        assert!(!parent.join(IDENTITY_MARKER).exists());
        assert!(!b.discard("thread").unwrap());
        assert!(b.names().unwrap().is_empty());
        assert!(
            b.remove_bounded("thread", usize::MAX).unwrap().complete,
            "an unmarked parent holds nothing to remove"
        );
        b.purge_trash().unwrap();

        let dir = a.prepare("thread").unwrap();
        fs::write(dir.join("file"), "keep").unwrap();
        assert!(b.discard("thread").is_err());
        assert!(b.prepare("thread").is_err());
        assert_eq!(fs::read_to_string(dir.join("file")).unwrap(), "keep");
        assert!(a.discard("thread").unwrap());
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
