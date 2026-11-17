//! Private, persistent attachment directories, independent of thread scratch.
//!
//! Only `files` is a sandbox mount. Directory handles pin every write to a
//! validated private directory; caller-supplied paths never reach these APIs.
use std::{
    fs::File,
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use rustix::fs::{AtFlags, Mode, OFlags, open, openat};
use sha2::{Digest, Sha256};

use crate::{ScratchRoot, ThreadReadDirectory};

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

fn component(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > 128
        || name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
    {
        return Err(invalid("invalid attachment store component"));
    }
    Ok(())
}

/// A directory opened without following its final component, owned by this uid
/// and accessible only to it. All child opens are relative to this pinned fd.
#[derive(Debug)]
pub struct PrivateDirectory {
    file: File,
    path: PathBuf,
}

impl PrivateDirectory {
    fn from_file(file: File, path: PathBuf) -> io::Result<Self> {
        let meta = file.metadata()?;
        if !meta.is_dir()
            || meta.uid() != rustix::process::geteuid().as_raw()
            || meta.mode() & 0o7777 != 0o700
        {
            return Err(invalid(
                "attachment directory must be owned by this uid with mode 0700",
            ));
        }
        let directory = Self { file, path };
        directory.validate_current()?;
        Ok(directory)
    }

    /// Open an existing private directory; never fixes an untrusted mode/owner.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::from(open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        Self::from_file(file, path.to_owned())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn validate_current(&self) -> io::Result<()> {
        let current = self.path.symlink_metadata()?;
        let held = self.file.metadata()?;
        if !current.is_dir()
            || current.uid() != rustix::process::geteuid().as_raw()
            || current.mode() & 0o7777 != 0o700
            || (current.dev(), current.ino()) != (held.dev(), held.ino())
        {
            return Err(invalid("attachment directory changed identity"));
        }
        Ok(())
    }

    pub fn directory(&self, name: &str) -> io::Result<Self> {
        component(name)?;
        self.validate_current()?;
        match rustix::fs::mkdirat(&self.file, name, Mode::from_raw_mode(0o700)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(error.into()),
        }
        let file = File::from(openat(
            &self.file,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        let child = Self::from_file(file, self.path.join(name))?;
        self.validate_current()?;
        Ok(child)
    }

    /// Lookup-only variant: quota/recovery scans must never recreate archived trees.
    pub fn existing_directory(&self, name: &str) -> io::Result<Self> {
        component(name)?;
        self.validate_current()?;
        let file = File::from(openat(
            &self.file,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        let child = Self::from_file(file, self.path.join(name))?;
        self.validate_current()?;
        Ok(child)
    }

    pub fn file(&self, name: &str, flags: OFlags) -> io::Result<File> {
        component(name)?;
        self.validate_current()?;
        let file = File::from(openat(
            &self.file,
            name,
            flags | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::from_raw_mode(0o600),
        )?);
        let meta = file.metadata()?;
        if !meta.is_file()
            || meta.uid() != rustix::process::geteuid().as_raw()
            || meta.mode() & 0o7777 != 0o600
            || meta.nlink() != 1
        {
            return Err(invalid("attachment file must be private and singly linked"));
        }
        self.validate_current()?;
        Ok(file)
    }

    pub fn remove_file(&self, name: &str) -> io::Result<()> {
        component(name)?;
        self.validate_current()?;
        match rustix::fs::unlinkat(&self.file, name, AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn rename_to(&self, name: &str, target: &Self, new_name: &str) -> io::Result<()> {
        component(name)?;
        component(new_name)?;
        self.validate_current()?;
        target.validate_current()?;
        rustix::fs::renameat(&self.file, name, &target.file, new_name)?;
        self.validate_current()?;
        target.validate_current()
    }

    /// Publish without replacing a previously committed directory entry.
    pub fn promote_to(&self, name: &str, target: &Self, new_name: &str) -> io::Result<()> {
        component(name)?;
        component(new_name)?;
        self.validate_current()?;
        target.validate_current()?;
        rustix::fs::renameat_with(
            &self.file,
            name,
            &target.file,
            new_name,
            rustix::fs::RenameFlags::NOREPLACE,
        )?;
        target.file.sync_all()?;
        self.file.sync_all()?;
        Ok(())
    }

    /// Enumerate through the pinned descriptor, not through a replaceable path.
    pub fn names(&self) -> io::Result<Vec<String>> {
        self.validate_current()?;
        let mut names = Vec::new();
        for entry in rustix::fs::Dir::read_from(&self.file)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .to_str()
                .map_err(|_| invalid("non-UTF8 store entry"))?;
            if name != "." && name != ".." {
                component(name)?;
                names.push(name.to_owned());
            }
        }
        self.validate_current()?;
        Ok(names)
    }

    pub fn read_identity(&self) -> io::Result<ThreadReadDirectory> {
        self.validate_current()?;
        let identity = ThreadReadDirectory::capture_private(&self.path)?;
        self.validate_current()?;
        Ok(identity)
    }
}

/// The scope is the canonical ownership root, never scratch contents or its
/// shortened directory name. Host and runtime share the full identity.
#[derive(Debug, Clone)]
pub struct AttachmentRoot {
    path: PathBuf,
}

impl AttachmentRoot {
    pub fn for_scope(scope: &Path) -> Self {
        Self {
            path: PathBuf::from("/var/tmp").join(format!(
                "evo-attachments-{}-{}",
                rustix::process::geteuid().as_raw(),
                digest(scope.as_os_str().as_encoded_bytes())
            )),
        }
    }

    /// A caller-owned private parent, useful for isolated tests.
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn thread_name(thread: &str) -> String {
        format!("t.{}", digest(thread.as_bytes()))
    }

    pub fn prepare(&self) -> io::Result<PrivateDirectory> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| invalid("store has no parent"))?;
        let name = self
            .path
            .file_name()
            .ok_or_else(|| invalid("store has no name"))?;
        let fd = open(
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        match rustix::fs::mkdirat(&fd, name, Mode::from_raw_mode(0o700)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(error.into()),
        }
        PrivateDirectory::open(&self.path)
    }

    pub fn thread(&self, thread: &str) -> io::Result<PrivateDirectory> {
        self.prepare()?.directory(&Self::thread_name(thread))
    }

    pub fn files(&self, thread: &str) -> io::Result<PrivateDirectory> {
        self.thread(thread)?.directory("files")
    }

    /// Locks live outside the removable thread tree, preventing lock replacement
    /// between an archive and a concurrent sender or unarchive.
    pub fn lock_thread(&self, thread: &str) -> io::Result<File> {
        self.lock(&Self::thread_name(thread))
    }

    pub fn try_lock_thread(&self, thread: &str) -> io::Result<File> {
        let file = self
            .prepare()?
            .directory("locks")?
            .file(&Self::thread_name(thread), OFlags::RDWR | OFlags::CREATE)?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)?;
        Ok(file)
    }

    pub fn lock(&self, name: &str) -> io::Result<File> {
        let file = self
            .prepare()?
            .directory("locks")?
            .file(name, OFlags::RDWR | OFlags::CREATE)?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)?;
        Ok(file)
    }

    /// Synchronous bounded deletion; a partial removal is an error, never success.
    /// Call while holding `lock_thread`; retry continues from the remaining tree.
    pub fn remove_thread(&self, thread: &str) -> io::Result<()> {
        let parent = self.prepare()?;
        let removal = ScratchRoot::at(self.path.clone())
            .remove_bounded(&Self::thread_name(thread), 4096)
            .map_err(io::Error::other)?;
        parent.validate_current()?;
        if !removal.complete
            || self
                .path
                .join(Self::thread_name(thread))
                .symlink_metadata()
                .is_ok()
        {
            return Err(io::Error::other(
                "attachment removal incomplete; retry archive",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        os::unix::fs::{PermissionsExt, symlink},
    };

    #[test]
    fn attachment_mount_identity_rejects_privacy_changes_and_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let root = AttachmentRoot::at(temp.path().join("attachments"));
        let files = root.files("thread").unwrap();
        let identity = files.read_identity().unwrap();
        assert!(identity.is_current());
        std::fs::set_permissions(files.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!identity.is_current());
        std::fs::set_permissions(files.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(identity.is_current());
        std::fs::rename(files.path(), files.path().with_file_name("previous")).unwrap();
        root.files("thread").unwrap();
        assert!(!identity.is_current());
    }

    #[test]
    fn independent_handles_keep_stable_paths_and_private_modes() {
        let temp = tempfile::tempdir().unwrap();
        let root = AttachmentRoot::at(temp.path().join("attachments"));
        let files = root.files("../../thread\n").unwrap();
        let path = files.path().to_owned();
        let mut file = files
            .file("a.pdf", OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL)
            .unwrap();
        file.write_all(b"pdf").unwrap();
        assert_eq!(file.metadata().unwrap().mode() & 0o777, 0o600);
        drop(root);
        let restored = AttachmentRoot::at(temp.path().join("attachments"));
        assert_eq!(restored.files("../../thread\n").unwrap().path(), path);
        assert_ne!(restored.files("another").unwrap().path(), path);
        assert_eq!(std::fs::read(path.join("a.pdf")).unwrap(), b"pdf");
    }

    #[test]
    fn rejects_traversal_symlinks_modes_and_replaced_directories() {
        let temp = tempfile::tempdir().unwrap();
        let root = AttachmentRoot::at(temp.path().join("attachments"));
        let files = root.files("thread").unwrap();
        for name in ["../bad", ".", "", "a\n", &"a".repeat(129)] {
            assert!(files.file(name, OFlags::CREATE | OFlags::WRONLY).is_err());
        }
        symlink(temp.path(), files.path().join("link")).unwrap();
        assert!(files.file("link", OFlags::RDONLY).is_err());
        std::fs::set_permissions(files.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(root.files("thread").is_err());
        std::fs::set_permissions(files.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::rename(files.path(), files.path().with_file_name("old")).unwrap();
        root.files("thread").unwrap();
        assert!(
            files
                .file("orphan", OFlags::WRONLY | OFlags::CREATE)
                .is_err()
        );
    }

    #[test]
    fn deletion_is_confirmed_and_does_not_follow_links() {
        let temp = tempfile::tempdir().unwrap();
        let root = AttachmentRoot::at(temp.path().join("attachments"));
        let files = root.files("thread").unwrap();
        let outside = temp.path().join("keep");
        std::fs::write(&outside, "keep").unwrap();
        symlink(&outside, files.path().join("link")).unwrap();
        let _lock = root.lock_thread("thread").unwrap();
        root.remove_thread("thread").unwrap();
        assert!(!files.path().exists());
        assert!(outside.exists());
        root.remove_thread("thread").unwrap();
    }
}
