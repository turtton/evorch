//! Private generation profiles. Unknown launching directories are never reused.
//! Once processes/workers are reclaimed, an atomic rename transfers ownership to
//! the persistent .reclaim-<uuid> queue, independently of all internal markers.
use rustix::fs::{FlockOperation, flock};
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
type Remover = fn(&Path) -> io::Result<()>;
pub struct Profile {
    pub path: PathBuf,
    _lease: File,
    launched: bool,
    remove: Remover,
}
const MARKER: &str = "evorch-renderer-profile-v1";
const RECLAIM: &str = ".reclaim-";
// Allow transient cleanup failures without disabling rendering, but retain at
// most eight failed generations per single-core profile root. At capacity,
// retry cleanup before allocating anything and fail closed if still full.
const MAX_RECLAIMS: usize = 8;
fn remove_tree(path: &Path) -> io::Result<()> {
    fs::remove_dir_all(path)
}
fn private_dir(path: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.mode() & 0o077 != 0 {
        return Err(io::Error::other(
            "renderer profile root must be a private directory",
        ));
    }
    Ok(())
}
impl Profile {
    pub fn create(root: &Path) -> io::Result<Self> {
        Self::create_with(root, remove_tree)
    }
    pub(super) fn create_with(root: &Path, remove: Remover) -> io::Result<Self> {
        private_dir(root)?;
        collect_with(root, remove)?;
        let path = root.join(uuid::Uuid::new_v4().to_string());
        private_dir(&path)?;
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path.join("owner.lock"))?;
        flock(&lease, FlockOperation::LockShared)?;
        fs::write(path.join("marker"), MARKER)?;
        Ok(Self {
            path,
            _lease: lease,
            launched: false,
            remove,
        })
    }
    pub fn mark_launched(&mut self) {
        self.launched = true;
    }
    pub fn confirm(&self) -> io::Result<()> {
        fs::write(self.path.join("confirmed"), MARKER)
    }
    /// Only after group/leader reclamation AND actual worker joins (or before
    /// spawn). Success means deletion OR durable transfer to the bounded queue.
    /// Rename/persistence failure retains this owner and prevents replacement.
    pub fn remove(&mut self) -> io::Result<()> {
        let root = self.path.parent().expect("generation root");
        let _ = retry_queued(root, self.remove);
        if quarantine(&mut self.path)? {
            let _ = remove_queued(&self.path, self.remove);
        }
        Ok(())
    }
}
fn is_reclaim(name: &str) -> bool {
    name.strip_prefix(RECLAIM)
        .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
}
fn quarantine(path: &mut PathBuf) -> io::Result<bool> {
    let result = (|| {
        let metadata = match fs::symlink_metadata(&*path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        if !metadata.is_dir() {
            return Err(io::Error::other(
                "renderer reclamation entry must be a directory",
            ));
        }
        let root = path.parent().expect("generation root").to_path_buf();
        let name = path.file_name().expect("generation name").to_string_lossy();
        if !is_reclaim(&name) {
            if uuid::Uuid::parse_str(&name).is_err() {
                return Err(io::Error::other("invalid renderer generation name"));
            }
            let queued = root.join(format!("{RECLAIM}{name}"));
            if fs::symlink_metadata(&queued).is_ok() {
                return Err(io::Error::other(
                    "renderer reclamation entry already exists",
                ));
            }
            match fs::rename(&*path, &queued) {
                Ok(()) => *path = queued,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(e),
            }
        }
        // Persist the transfer BEFORE deleting marker/owner.lock. The queue
        // entry itself disappears only after the whole directory is removed.
        File::open(root)?.sync_all()?;
        Ok(true)
    })();
    if let Err(error) = &result {
        tracing::warn!(profile = %path.display(), %error, "renderer profile isolation failed; retaining owner");
    }
    result
}
fn remove_queued(path: &Path, remove: Remover) -> io::Result<()> {
    let result = (|| {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        if !metadata.is_dir() {
            return Err(io::Error::other("invalid renderer reclamation record"));
        }
        let root = path.parent().expect("reclamation root");
        File::open(root)?.sync_all()?;
        match remove(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        File::open(root)?.sync_all()
    })();
    if let Err(error) = &result {
        tracing::warn!(profile = %path.display(), %error, "renderer profile deletion failed; queued for retry");
    }
    result
}
fn retry_queued(root: &Path, remove: Remover) -> io::Result<usize> {
    let mut remaining = 0;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if is_reclaim(&entry.file_name().to_string_lossy()) {
            // Never follow a corrupt/symlink record to unrelated data.
            if !entry.file_type()?.is_dir() {
                return Err(io::Error::other("invalid renderer reclamation record"));
            }
            if remove_queued(&entry.path(), remove).is_err() {
                remaining += 1;
            }
        }
    }
    Ok(remaining)
}
#[cfg(test)]
pub fn collect(root: &Path) -> io::Result<()> {
    collect_with(root, remove_tree)
}
fn collect_with(root: &Path, remove: Remover) -> io::Result<()> {
    let _ = retry_queued(root, remove)?;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if uuid::Uuid::parse_str(&entry.file_name().to_string_lossy()).is_err()
            || !entry.file_type()?.is_dir()
        {
            continue;
        }
        let mut path = entry.path();
        if fs::read_to_string(path.join("marker")).ok().as_deref() != Some(MARKER)
            || fs::read_to_string(path.join("confirmed")).ok().as_deref() != Some(MARKER)
        {
            continue;
        }
        let Ok(lease) = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(path.join("owner.lock"))
        else {
            continue;
        };
        // A helper's shared lease protects it even after abrupt core death.
        if flock(&lease, FlockOperation::NonBlockingLockExclusive).is_ok() && quarantine(&mut path)?
        {
            let _ = remove_queued(&path, remove);
        }
    }
    if retry_queued(root, remove)? >= MAX_RECLAIMS {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "renderer profile reclamation queue is full",
        ));
    }
    Ok(())
}
impl Drop for Profile {
    fn drop(&mut self) {
        // Before spawn there are no users; transfer ownership even on partial
        // failure. Unknown launched generations remain quarantined, never reused.
        if !self.launched {
            let _ = self.remove();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn collection_preserves_live_unknown_and_unrelated_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("profiles");
        let mut active = Profile::create(&root).unwrap();
        active.mark_launched();
        active.confirm().unwrap();
        let active_path = active.path.clone();
        collect(&root).unwrap();
        assert!(active_path.exists());
        let mut unknown = Profile::create(&root).unwrap();
        unknown.mark_launched();
        let unknown_path = unknown.path.clone();
        drop(unknown);
        let unrelated = root.join("not-a-generation");
        fs::create_dir(&unrelated).unwrap();
        fs::write(unrelated.join("sentinel"), "keep").unwrap();
        std::os::unix::fs::symlink(&unrelated, root.join(uuid::Uuid::new_v4().to_string()))
            .unwrap();
        drop(active);
        collect(&root).unwrap();
        assert!(!active_path.exists());
        assert!(unknown_path.exists());
        assert_eq!(
            fs::read_to_string(unrelated.join("sentinel")).unwrap(),
            "keep"
        );
        let not_launched = Profile::create(&root).unwrap();
        let path = not_launched.path.clone();
        drop(not_launched);
        assert!(!path.exists());
    }

    fn fail_partial(path: &Path) -> io::Result<()> {
        for name in ["marker", "confirmed", "owner.lock"] {
            match fs::remove_file(path.join(name)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "partial deletion",
        ))
    }
    #[test]
    fn partial_deletion_allows_next_generations_but_bounds_persistent_recovery_queue() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("profiles");
        for count in 1..=MAX_RECLAIMS {
            let mut profile = Profile::create_with(&root, fail_partial).unwrap();
            profile.mark_launched();
            profile.confirm().unwrap();
            let original = profile.path.clone();
            fs::write(original.join("remaining-data"), "private").unwrap();
            profile.remove().unwrap(); // Durable isolation, not successful deletion.
            assert!(!original.exists());
            assert!(is_reclaim(
                &profile.path.file_name().unwrap().to_string_lossy()
            ));
            assert!(profile.path.join("remaining-data").exists());
            for name in ["marker", "confirmed", "owner.lock"] {
                assert!(!profile.path.join(name).exists());
            }
            drop(profile); // Recovery no longer depends on in-memory ownership.
            assert_eq!(fs::read_dir(&root).unwrap().count(), count);
        }
        for _ in 0..4 {
            assert_eq!(
                Profile::create_with(&root, fail_partial)
                    .err()
                    .unwrap()
                    .kind(),
                io::ErrorKind::WouldBlock
            );
            assert_eq!(fs::read_dir(&root).unwrap().count(), MAX_RECLAIMS);
        }
        let next = Profile::create(&root).unwrap(); // Recovery before allocation.
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        drop(next);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    }

    #[test]
    fn invalid_reclamation_record_blocks_new_profiles_without_following_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("profiles");
        private_dir(&root).unwrap();
        let unrelated = temp.path().join("unrelated");
        fs::create_dir(&unrelated).unwrap();
        fs::write(unrelated.join("sentinel"), "keep").unwrap();
        let queued = root.join(format!("{RECLAIM}{}", uuid::Uuid::new_v4()));
        std::os::unix::fs::symlink(&unrelated, &queued).unwrap();
        for _ in 0..4 {
            assert!(Profile::create(&root).is_err());
            assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
            assert!(unrelated.join("sentinel").exists());
        }
        fs::remove_file(queued).unwrap();
        drop(Profile::create(&root).unwrap());
        assert_eq!(fs::read_dir(root).unwrap().count(), 0);
    }

    #[test]
    fn failed_isolation_keeps_original_owner_and_never_deletes_before_transfer() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("profiles");
        let mut profile = Profile::create(&root).unwrap();
        profile.mark_launched();
        profile.confirm().unwrap();
        let original = profile.path.clone();
        let collision = root.join(format!(
            "{RECLAIM}{}",
            original.file_name().unwrap().to_string_lossy()
        ));
        let unrelated = temp.path().join("unrelated");
        fs::create_dir(&unrelated).unwrap();
        fs::write(unrelated.join("sentinel"), "keep").unwrap();
        std::os::unix::fs::symlink(&unrelated, &collision).unwrap();
        assert!(profile.remove().is_err());
        assert_eq!(profile.path, original);
        for name in ["marker", "confirmed", "owner.lock"] {
            assert!(original.join(name).exists());
        }
        assert!(unrelated.join("sentinel").exists());
        fs::remove_file(collision).unwrap();
        profile.remove().unwrap();
        assert!(!original.exists());
    }
}
