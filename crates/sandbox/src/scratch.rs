//! Disk-backed temporary command files shared by one sandbox instance.
//!
//! Dropping a sandbox never scans or recursively deletes on the caller's thread.
//! A bounded queue and one throttled worker remove expired scratch trees. Crashed
//! processes / full queues leave /var/tmp cleanup to the OS.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, mpsc};
use std::time::Duration;

#[derive(Debug)]
pub(super) struct Scratch {
    path: PathBuf,
}

impl Scratch {
    pub(super) fn new() -> std::io::Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("evorch-scratch-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in("/var/tmp")?;
        // TempDir is private (0700). Keep HOME and Cargo's lock directory on disk.
        fs::create_dir_all(directory.path().join("home/.cargo"))?;
        let path = directory.keep();
        Ok(Self { path })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        static CLEANUP: OnceLock<Option<mpsc::SyncSender<PathBuf>>> = OnceLock::new();
        let sender = CLEANUP.get_or_init(|| {
            let (sender, receiver) = mpsc::sync_channel::<PathBuf>(8);
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
            let _ = sender.try_send(self.path.clone());
        }
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
}
