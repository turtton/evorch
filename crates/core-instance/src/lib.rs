//! Config-scoped discovery and launch -> lifetime locking. No runtime, DB or GUI.
//! Locks are released only by close; lock files are never unlinked.
#[cfg(target_os = "linux")]
pub mod handoff;

use rustix::fs::{FlockOperation, OFlags, flock};
use sha2::{Digest, Sha256};
mod handoff_proof;
pub use handoff_proof::InheritedHandoffProof;

/// Synthetic physical evidence for recovery tests only. Never enabled by default.
#[cfg(feature = "recovery-test-support")]
pub mod test_support {
    use super::InheritedHandoffProof;
    pub fn inherited_handoff_proof(
        restart_id: String,
        config_identity: String,
        successor_epoch: u64,
        nonce_hash: String,
        successor_build: String,
    ) -> InheritedHandoffProof {
        InheritedHandoffProof::new(
            restart_id,
            config_identity,
            successor_epoch,
            nonce_hash,
            successor_build,
        )
    }
}

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, BorrowedFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command},
};

#[derive(Debug, thiserror::Error)]
pub enum InstanceError {
    #[error("evorch-core is already running (or starting) for this config directory")]
    AlreadyRunning,
    #[error(transparent)]
    Io(#[from] io::Error),
}
fn unsafe_path() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "unsafe core IPC path")
}
fn check_meta(m: &fs::Metadata, directory: bool) -> io::Result<()> {
    if m.uid() != rustix::process::geteuid().as_raw()
        || m.file_type().is_symlink()
        || (directory && !m.is_dir())
        || m.mode() & 0o7777 != if directory { 0o700 } else { 0o600 }
    {
        return Err(unsafe_path());
    }
    Ok(())
}
fn check(path: &Path, directory: bool) -> io::Result<()> {
    check_meta(&fs::symlink_metadata(path)?, directory)
}
fn regular(file: &File, path: &Path) -> io::Result<()> {
    let m = file.metadata()?;
    check_meta(&m, false)?;
    let p = fs::symlink_metadata(path)?;
    if !m.is_file() || m.nlink() != 1 || m.dev() != p.dev() || m.ino() != p.ino() {
        return Err(unsafe_path());
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
        .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC).bits() as i32)
        .open(path)?;
    regular(&f, path)?;
    flock(&f, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
        if e == rustix::io::Errno::WOULDBLOCK {
            InstanceError::AlreadyRunning
        } else {
            InstanceError::Io(e.into())
        }
    })?;
    Ok(f)
}

#[derive(Debug, Clone)]
pub struct ConfigIdentity {
    config: PathBuf,
    identity: String,
}
impl ConfigIdentity {
    pub fn new(config: &Path) -> io::Result<Self> {
        fs::create_dir_all(config)?;
        let config = fs::canonicalize(config)?;
        let m = fs::metadata(&config)?;
        if !m.is_dir() || m.uid() != rustix::process::geteuid().as_raw() || m.mode() & 0o022 != 0 {
            return Err(unsafe_path());
        }
        let identity = format!("{:x}", Sha256::digest(config.as_os_str().as_bytes()));
        Ok(Self { config, identity })
    }
    pub fn config_dir(&self) -> &Path {
        &self.config
    }
    pub fn identity(&self) -> &str {
        &self.identity
    }
    pub fn socket(&self, parent: &Path) -> io::Result<PathBuf> {
        if !parent.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "IPC parent must be absolute",
            ));
        }
        let parent = fs::canonicalize(parent)?;
        let dir = parent.join(format!(
            "evorch-core-{}",
            rustix::process::geteuid().as_raw()
        ));
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e),
        }
        check(&dir, true)?;
        let socket = dir.join(format!("{}.sock", &self.identity[..32]));
        if socket.as_os_str().as_bytes().len() >= 104 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "core IPC parent is too long",
            ));
        }
        Ok(socket)
    }
    pub fn socket_with_runtime_dir(
        &self,
        runtime: Option<&Path>,
        fallback: &Path,
    ) -> io::Result<PathBuf> {
        let parent = match runtime {
            Some(p) => {
                if !p.is_absolute() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "XDG_RUNTIME_DIR must be absolute",
                    ));
                }
                check(p, true)?;
                fs::canonicalize(p)?
            }
            None => fallback.to_path_buf(),
        };
        self.socket(&parent)
    }
    pub fn default_socket(&self) -> io::Result<PathBuf> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        self.socket_with_runtime_dir(runtime.as_deref(), Path::new("/tmp"))
    }
    fn launch_path(&self) -> PathBuf {
        self.config.join(".evorch-core.launch")
    }
    fn lifetime_path(&self) -> PathBuf {
        self.config.join(".evorch-core.lock")
    }
    pub fn record_path(&self) -> PathBuf {
        self.config.join(".evorch-core.endpoint")
    }
    /// Bounded hint, never evidence of a live owner. Corrupt/old formats are
    /// ignored; unsafe filesystem objects are not followed.
    pub fn read_record(&self) -> io::Result<Option<EndpointRecord>> {
        let p = self.record_path();
        let f = match OpenOptions::new()
            .read(true)
            .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC).bits() as i32)
            .open(&p)
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        regular(&f, &p)?;
        let mut data = Vec::new();
        f.take(8193).read_to_end(&mut data)?;
        if data.len() > 8192 {
            return Ok(None);
        }
        let Ok(r) = serde_json::from_slice::<EndpointRecord>(&data) else {
            return Ok(None);
        };
        if r.identity != self.identity || r.epoch == 0 {
            return Ok(None);
        }
        let expected = format!("{}.sock", &self.identity[..32]);
        let directory = format!("evorch-core-{}", rustix::process::geteuid().as_raw());
        if !r.socket.is_absolute()
            || r.socket.file_name() != Some(std::ffi::OsStr::new(&expected))
            || r.socket.parent().and_then(Path::file_name) != Some(std::ffi::OsStr::new(&directory))
            || r.socket
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(unsafe_path());
        }
        Ok(Some(r))
    }
}
/// Refuse symlinked/untrusted socket directories even for a saved endpoint.
pub fn validate_endpoint_path(path: &Path) -> io::Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(unsafe_path());
    }
    let parent = path.parent().ok_or_else(unsafe_path)?;
    check(parent, true)?;
    if fs::canonicalize(parent)? != parent {
        return Err(unsafe_path());
    }
    Ok(())
}
pub fn validate_socket(path: &Path) -> io::Result<()> {
    validate_endpoint_path(path)?;
    check(path, false)?;
    if !fs::symlink_metadata(path)?.file_type().is_socket() {
        return Err(unsafe_path());
    }
    Ok(())
}

/// Listen on a Unix socket at `path` whose file is 0600 from the moment it exists.
///
/// bind(2) creates the socket file with a mode masked by the umask, so binding
/// and then chmod-ing leaves a window in which the file is wider (0755 under the
/// usual umask) at its public path. A peer that connects in that window refuses
/// it as an unsafe path ([`validate_socket`]). The umask is process-wide and not
/// safe to change while other threads create files, so the socket's own mode is
/// narrowed before bind instead, which bind then uses. Like a plain bind, this
/// fails if `path` already exists.
pub fn bind_private_socket(path: &Path) -> io::Result<std::os::unix::net::UnixListener> {
    let listener = bind_owner_only(path)?;
    // A umask that also clears owner bits still narrows the mode: widen it back
    // to 0600 (never beyond), as the socket's peers require exactly that.
    if fs::symlink_metadata(path)?.mode() & 0o7777 != 0o600 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(listener)
}

/// socket, fchmod 0600, bind, listen: on Linux the file is created with the
/// socket's mode (masked by the umask), so it never appears wider than 0600.
#[cfg(target_os = "linux")]
fn bind_owner_only(path: &Path) -> io::Result<std::os::unix::net::UnixListener> {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
    let socket = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )?;
    rustix::fs::fchmod(&socket, rustix::fs::Mode::from_raw_mode(0o600))?;
    rustix::net::bind(&socket, &SocketAddrUnix::new(path)?)?;
    // As std's UnixListener::bind on Linux: the system's maximum backlog.
    rustix::net::listen(&socket, -1)?;
    Ok(std::os::unix::net::UnixListener::from(socket))
}

/// Elsewhere bind(2) ignores the socket's own mode (and fchmod on a socket
/// fails), so the file is narrowed after bind, as before.
#[cfg(not(target_os = "linux"))]
fn bind_owner_only(path: &Path) -> io::Result<std::os::unix::net::UnixListener> {
    std::os::unix::net::UnixListener::bind(path)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointRecord {
    pub identity: String,
    pub socket: PathBuf,
    pub epoch: u64,
}

/// Exclusive election, held from the second connection check through ready.
/// No Clone: release is close, NEVER LOCK_UN (the child shares this OFD).
pub struct LaunchGuard {
    config: ConfigIdentity,
    file: File,
}
impl LaunchGuard {
    pub fn acquire(config: &ConfigIdentity) -> Result<Self, InstanceError> {
        Ok(Self {
            config: config.clone(),
            file: lock(&config.launch_path())?,
        })
    }
    pub fn config(&self) -> &ConfigIdentity {
        &self.config
    }
    /// The temporary lifetime FD is closed before returning, before any spawn.
    pub fn lifetime_available(&self) -> Result<bool, InstanceError> {
        match lock(&self.config.lifetime_path()) {
            Ok(f) => {
                drop(f);
                Ok(true)
            }
            Err(InstanceError::AlreadyRunning) => Ok(false),
            Err(e) => Err(e),
        }
    }
    /// Only the forked child clears CLOEXEC. Concurrent tool/CEF/shell spawns
    /// in the parent cannot leak the lock.
    pub fn spawn(&self, mut command: Command) -> io::Result<Child> {
        let fd = self.file.as_raw_fd();
        command.arg("--launch-fd").arg(fd.to_string());
        // SAFETY: only async-signal-safe fcntl; self keeps fd alive through exec.
        unsafe {
            command.pre_exec(move || {
                rustix::io::fcntl_setfd(BorrowedFd::borrow_raw(fd), rustix::io::FdFlags::empty())?;
                Ok(())
            });
        }
        command.spawn()
    }
    /// Adopt without reopening/reflocking; restore CLOEXEC before other work.
    ///
    /// # Safety
    /// fd must be an exclusively owned, valid inherited descriptor, not owned
    /// by any Rust object. Call at process entry before starting other threads.
    pub unsafe fn adopt(config: &ConfigIdentity, fd: i32) -> io::Result<Self> {
        if fd < 3 {
            return Err(unsafe_path());
        }
        // Check existence before constructing an OwnedFd from untrusted CLI input.
        fs::metadata(format!("/proc/self/fd/{fd}"))?;
        // SAFETY: caller transfers exclusive ownership.
        let file = unsafe { File::from_raw_fd(fd) };
        rustix::io::fcntl_setfd(&file, rustix::io::FdFlags::CLOEXEC)?;
        regular(&file, &config.launch_path())?;
        if !rustix::fs::fcntl_getfl(&file)?.contains(OFlags::RDWR) {
            return Err(unsafe_path());
        }
        // Linux reports locks of this OFD, not just locks of some reused PID.
        #[cfg(target_os = "linux")]
        if !fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))?
            .lines()
            .any(|l| l.starts_with("lock:") && l.contains(" FLOCK ") && l.contains(" WRITE "))
        {
            return Err(unsafe_path());
        }
        #[cfg(not(target_os = "linux"))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "launch adoption requires Linux fdinfo",
        ));
        Ok(Self {
            config: config.clone(),
            file,
        })
    }
}

pub struct Instance {
    pub identity: String,
    pub socket: PathBuf,
    config: ConfigIdentity,
    _lifetime: File,
}
impl Instance {
    /// Guard is a capability: no lifetime acquisition without launch.
    pub fn acquire(launch: &LaunchGuard, ipc_parent: &Path) -> Result<Self, InstanceError> {
        let config = launch.config.clone();
        let lifetime = lock(&config.lifetime_path())?;
        let socket = config.socket(ipc_parent)?;
        // A previous owner may have used another XDG_RUNTIME_DIR.
        if let Some(record) = config.read_record()? {
            reap_socket(&record.socket)?;
        }
        reap_socket(&socket)?;
        Ok(Self {
            identity: config.identity.clone(),
            socket,
            config,
            _lifetime: lifetime,
        })
    }
    pub fn acquire_with_runtime_dir(
        launch: &LaunchGuard,
        runtime: Option<&Path>,
        fallback: &Path,
    ) -> Result<Self, InstanceError> {
        let socket = launch.config.socket_with_runtime_dir(runtime, fallback)?;
        let parent = socket
            .parent()
            .and_then(Path::parent)
            .ok_or_else(unsafe_path)?;
        Self::acquire(launch, parent)
    }
    pub fn acquire_default(launch: &LaunchGuard) -> Result<Self, InstanceError> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        Self::acquire_with_runtime_dir(launch, runtime.as_deref(), Path::new("/tmp"))
    }
    pub fn bind(&self) -> io::Result<std::os::unix::net::UnixListener> {
        bind_private_socket(&self.socket)
    }
    /// Publish only after listener bind. Atomic rename prevents partial hints.
    pub fn publish(&self, epoch: u64) -> io::Result<()> {
        if epoch == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "zero epoch"));
        }
        validate_socket(&self.socket)?;
        let mut tmp = tempfile::NamedTempFile::new_in(&self.config.config)?;
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        let r = EndpointRecord {
            identity: self.identity.clone(),
            socket: self.socket.clone(),
            epoch,
        };
        tmp.write_all(&serde_json::to_vec(&r)?)?;
        tmp.as_file().sync_all()?;
        tmp.persist(self.config.record_path())
            .map_err(|e| e.error)?;
        Ok(())
    }
}
fn reap_socket(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            validate_socket(path)?;
            fs::remove_file(path)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
impl Drop for Instance {
    fn drop(&mut self) {
        if validate_socket(&self.socket).is_ok() {
            let _ = fs::remove_file(&self.socket);
        }
    }
}

/// Exclusion of a core over one config directory, for offline maintenance that must be
/// sure no core runs or starts. It holds the launch and the lifetime lock of the directory
/// (in the order a starting core takes them) until it is dropped; a core that tries to
/// start meanwhile is refused as already running.
///
/// It does not discover a core by endpoint or process: the locks are the proof. Lock files
/// are never unlinked or replaced.
#[derive(Debug)]
pub struct MaintenanceGuard {
    config: PathBuf,
    _launch: Option<File>,
    _lifetime: Option<File>,
}

/// The config directory as it exists: never created, and as private as a core demands.
fn existing_config(config: &Path) -> io::Result<PathBuf> {
    let config = fs::canonicalize(config)?;
    let m = fs::metadata(&config)?;
    if !m.is_dir() || m.uid() != rustix::process::geteuid().as_raw() || m.mode() & 0o022 != 0 {
        return Err(unsafe_path());
    }
    Ok(config)
}

fn probe_shared(path: &Path) -> Result<Option<File>, InstanceError> {
    let f = match OpenOptions::new()
        .read(true)
        .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC).bits() as i32)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    regular(&f, path)?;
    flock(&f, FlockOperation::NonBlockingLockShared).map_err(|e| {
        if e == rustix::io::Errno::WOULDBLOCK {
            InstanceError::AlreadyRunning
        } else {
            InstanceError::Io(e.into())
        }
    })?;
    Ok(Some(f))
}

impl MaintenanceGuard {
    /// For a preview: creates and changes nothing. Existing lock files are opened
    /// read-only and locked shared without waiting; a missing lock file means no core ever
    /// used the directory, which is not an error here (the database lease still decides).
    ///
    /// # Errors
    /// [`InstanceError::AlreadyRunning`] while a core holds a lock; I/O errors for a
    /// missing or unsafe directory, a symlinked or foreign lock file.
    pub fn inspect(config: &Path) -> Result<Self, InstanceError> {
        let config = existing_config(config)?;
        let identity = ConfigIdentity {
            identity: String::new(),
            config: config.clone(),
        };
        let launch = probe_shared(&identity.launch_path())?;
        let lifetime = probe_shared(&identity.lifetime_path())?;
        Ok(Self {
            config,
            _launch: launch,
            _lifetime: lifetime,
        })
    }

    /// For an apply: both locks exclusively without waiting, creating a lock file that does
    /// not exist yet (owner only). The directory itself must already exist.
    ///
    /// # Errors
    /// As [`Self::inspect`].
    pub fn acquire(config: &Path) -> Result<Self, InstanceError> {
        let config = existing_config(config)?;
        let identity = ConfigIdentity {
            identity: String::new(),
            config: config.clone(),
        };
        let launch = lock(&identity.launch_path())?;
        let lifetime = lock(&identity.lifetime_path())?;
        Ok(Self {
            config,
            _launch: Some(launch),
            _lifetime: Some(lifetime),
        })
    }

    pub fn config_dir(&self) -> &Path {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Under an empty umask a plain bind creates the file 0777; this one must
    /// still create it 0600, with no chmod afterwards to hide a window.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_private_socket_is_created_owner_only_whatever_the_umask() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s");
        let previous = rustix::process::umask(rustix::fs::Mode::empty());
        let bound = bind_owner_only(&path);
        rustix::process::umask(previous);
        let _listener = bound.unwrap();
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.mode() & 0o7777, 0o600);
        assert!(
            bind_private_socket(&path).is_err(),
            "an existing path is never replaced"
        );
    }

    #[test]
    fn inspect_changes_nothing_and_a_missing_lock_means_no_core() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let guard = MaintenanceGuard::inspect(dir.path()).unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
        drop(guard);
        assert!(MaintenanceGuard::inspect(&dir.path().join("absent")).is_err());
    }

    #[test]
    fn a_running_or_starting_core_is_refused_by_both_modes_and_blocks_a_new_one() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let config = ConfigIdentity::new(dir.path()).unwrap();
        let launch = LaunchGuard::acquire(&config).unwrap();
        assert!(matches!(
            MaintenanceGuard::inspect(dir.path()),
            Err(InstanceError::AlreadyRunning)
        ));
        assert!(matches!(
            MaintenanceGuard::acquire(dir.path()),
            Err(InstanceError::AlreadyRunning)
        ));
        drop(launch);
        let held = MaintenanceGuard::acquire(dir.path()).unwrap();
        assert!(matches!(
            LaunchGuard::acquire(&config),
            Err(InstanceError::AlreadyRunning)
        ));
        let lifetime_probe = MaintenanceGuard::inspect(dir.path());
        assert!(matches!(lifetime_probe, Err(InstanceError::AlreadyRunning)));
        drop(held);
        MaintenanceGuard::inspect(dir.path()).unwrap();
    }

    #[test]
    fn a_symlinked_lock_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let target = dir.path().join("elsewhere");
        fs::write(&target, b"").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join(".evorch-core.lock")).unwrap();
        assert!(MaintenanceGuard::inspect(dir.path()).is_err());
        assert!(MaintenanceGuard::acquire(dir.path()).is_err());
    }
}
