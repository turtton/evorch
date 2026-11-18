//! Private generation profiles. Only confirmed, unlocked generations are removed.
//! Unknown/incomplete directories are left in place (quarantined, never reused).
use rustix::fs::{FlockOperation, flock};
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
pub struct Profile {
    pub path: PathBuf,
    _lease: File,
    launched: bool,
}
const MARKER: &str = "evorch-renderer-profile-v1";
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
        private_dir(root)?;
        collect(root)?;
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
        })
    }
    pub fn mark_launched(&mut self) {
        self.launched = true;
    }
    pub fn confirm(&self) -> io::Result<()> {
        fs::write(self.path.join("confirmed"), MARKER)
    }
    /// Call only after the owned group has been killed and its leader reaped.
    pub fn remove(self) -> io::Result<()> {
        match fs::remove_dir_all(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}
pub fn collect(root: &Path) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if uuid::Uuid::parse_str(&entry.file_name().to_string_lossy()).is_err()
            || !entry.file_type()?.is_dir()
        {
            continue;
        }
        let path = entry.path();
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
        // The helper also holds a shared lease: a dead core does not make an
        // active helper's profile eligible. Unknown launching profiles stay put.
        if flock(&lease, FlockOperation::NonBlockingLockExclusive).is_ok() {
            fs::remove_dir_all(path)?;
        }
    }
    Ok(())
}

impl Drop for Profile {
    fn drop(&mut self) {
        // Before spawn no child could hold this directory. Unknown launched
        // generations remain quarantined unless Hello confirmed ownership.
        if !self.launched {
            let _ = fs::remove_dir_all(&self.path);
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
}
