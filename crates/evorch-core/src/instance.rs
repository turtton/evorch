//! Lifetime flock, independent startup arbitration and private, short IPC paths.
use rustix::fs::{FlockOperation, flock};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

#[derive(Debug, thiserror::Error)]
pub enum InstanceError {
    #[error("evorch-core is already running (or starting) for this config directory")]
    AlreadyRunning,
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub struct Instance {
    pub identity: String,
    pub socket: PathBuf,
    _lifetime: File,
}
fn check(path: &Path, directory: bool) -> io::Result<()> {
    let m = fs::symlink_metadata(path)?;
    if m.uid() != rustix::process::geteuid().as_raw()
        || m.file_type().is_symlink()
        || (directory && !m.is_dir())
        || m.mode() & 0o777 != if directory { 0o700 } else { 0o600 }
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe core IPC path",
        ));
    }
    Ok(())
}
fn lock(path: &Path) -> Result<File, InstanceError> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)?;
    check(path, false)?;
    if !f.metadata()?.is_file() || f.metadata()?.nlink() != 1 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "unsafe core lock").into());
    }
    flock(&f, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
        if e == rustix::io::Errno::WOULDBLOCK {
            InstanceError::AlreadyRunning
        } else {
            InstanceError::Io(e.into())
        }
    })?;
    Ok(f)
}
impl Instance {
    /// An explicitly configured runtime directory must be safe; never silently
    /// change namespaces on an invalid value. Tests pass paths, not process env.
    pub fn acquire_with_runtime_dir(
        config_dir: &Path,
        runtime_dir: Option<&Path>,
        fallback: &Path,
    ) -> Result<Self, InstanceError> {
        let parent = match runtime_dir {
            Some(path) => {
                if !path.is_absolute() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "XDG_RUNTIME_DIR must be absolute",
                    )
                    .into());
                }
                check(path, true)?;
                fs::canonicalize(path)?
            }
            None => fallback.to_path_buf(),
        };
        Self::acquire(config_dir, &parent)
    }

    pub fn acquire_default(config_dir: &Path) -> Result<Self, InstanceError> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        Self::acquire_with_runtime_dir(config_dir, runtime.as_deref(), Path::new("/tmp"))
    }

    /// Must run before opening storage. Aliases canonicalise to the same identity.
    pub fn acquire(config_dir: &Path, ipc_parent: &Path) -> Result<Self, InstanceError> {
        fs::create_dir_all(config_dir)?;
        let canonical = fs::canonicalize(config_dir)?;
        let identity = format!("{:x}", Sha256::digest(canonical.as_os_str().as_bytes()));
        // Locks belong to the database location, not the runtime directory:
        // different XDG_RUNTIME_DIR settings must not create two owners.
        let _startup = lock(&canonical.join(".evorch-core.start"))?;
        let lifetime = lock(&canonical.join(".evorch-core.lock"))?;
        let dir = ipc_parent.join(format!(
            "evorch-core-{}",
            rustix::process::geteuid().as_raw()
        ));
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
        check(&dir, true)?;
        let prefix = &identity[..32];
        let socket = dir.join(format!("{prefix}.sock"));
        if socket.as_os_str().as_bytes().len() >= 104 {
            return Err(
                io::Error::new(io::ErrorKind::InvalidInput, "core IPC parent is too long").into(),
            );
        }
        match fs::symlink_metadata(&socket) {
            Ok(m) => {
                check(&socket, false)?;
                if !m.file_type().is_socket() {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "core socket path is not a socket",
                    )
                    .into());
                }
                // Only the lifetime lock proves staleness; never probe socket existence.
                fs::remove_file(&socket)?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        Ok(Self {
            identity,
            socket,
            _lifetime: lifetime,
        })
    }
    pub fn bind(&self) -> io::Result<tokio::net::UnixListener> {
        let listener = tokio::net::UnixListener::bind(&self.socket)?;
        fs::set_permissions(&self.socket, fs::Permissions::from_mode(0o600))?;
        Ok(listener)
    }
}
impl Drop for Instance {
    fn drop(&mut self) {
        // Lock files deliberately remain: unlinking them creates split-lock races.
        if check(&self.socket, false).is_ok() {
            let _ = fs::remove_file(&self.socket);
        }
    }
}
