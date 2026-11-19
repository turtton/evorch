//! Linux-only exec boundary. This module never opens a database or acquires a lock.
//! Call entry inspection before initializing an executor, database, or producer.
use crate::{ConfigIdentity, InheritedHandoffProof, Instance, reap_socket, regular, unsafe_path};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    convert::Infallible,
    ffi::{CString, OsString},
    fs::{self, File},
    io::{self, Read, Write},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{FileExt, MetadataExt, PermissionsExt},
        },
    },
    path::Path,
};

#[path = "handoff_boot.rs"]
pub mod boot;
#[path = "handoff_entry.rs"]
pub mod entry;
#[path = "handoff_receipt.rs"]
pub mod receipt;

const SEALS: i32 = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
const TICKET_LIMIT: u64 = 8192;

/// Values read back from the durable transaction by the integrator. A serialized
/// record alone grants no authority: the inherited sealed FD and lock are required.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffRecord {
    pub restart_id: String,
    pub config_identity: String,
    pub predecessor_epoch: u64,
    pub successor_epoch: u64,
    pub nonce_hash: String,
    pub successor_build: String,
    pub boot_deadline: boot::BootDeadline,
}
impl HandoffRecord {
    fn validate(&self) -> io::Result<()> {
        self.boot_deadline.validate()?;
        if self.successor_epoch <= self.predecessor_epoch
            || [
                &self.restart_id,
                &self.config_identity,
                &self.nonce_hash,
                &self.successor_build,
            ]
            .iter()
            .any(|s| s.is_empty() || s.len() > 256 || s.contains('\0'))
        {
            return Err(unsafe_path());
        }
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ticket {
    version: u32,
    pid: u32,
    device: u64,
    inode: u64,
    record: HandoffRecord,
}

/// Establish the shutdown-signal policy before creating the first thread.
/// All later threads inherit this mask. Use only signalfd/sigwait, not handlers;
/// stop the inbox reader under the final fence so it cannot consume a shutdown
/// concurrently with exec. There is deliberately no Drop that unblocks signals.
pub struct BlockedShutdownSignals {
    _private: (),
}
#[derive(Debug, PartialEq, Eq)]
pub enum ShutdownSignal {
    Interrupt,
    Terminate,
}
pub struct ShutdownInbox(File);
impl BlockedShutdownSignals {
    pub fn at_entry() -> io::Result<Self> {
        if fs::read_dir("/proc/self/task")?.count() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "block shutdown signals before creating threads",
            ));
        }
        let mut mask = mask_now()?;
        unsafe {
            libc::sigaddset(&mut mask, libc::SIGINT);
            libc::sigaddset(&mut mask, libc::SIGTERM);
        }
        set_mask(&mask)?;
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = libc::SIG_DFL;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
        }
        for signal in [libc::SIGINT, libc::SIGTERM] {
            cvt(unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) })?;
        }
        Ok(Self { _private: () })
    }
    /// Create AFTER inherited-FD inspection in a successor. The FD itself is
    /// CLOEXEC, never on the inheritance allowlist. The mask stays blocked.
    pub fn inbox(&self) -> io::Result<ShutdownInbox> {
        let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut mask);
            libc::sigaddset(&mut mask, libc::SIGINT);
            libc::sigaddset(&mut mask, libc::SIGTERM);
        }
        let fd = cvt(unsafe { libc::signalfd(-1, &mask, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) })?;
        Ok(ShutdownInbox(unsafe { File::from_raw_fd(fd) }))
    }
    fn verify_masks(&self) -> io::Result<()> {
        for signal in [libc::SIGINT, libc::SIGTERM] {
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            cvt(unsafe { libc::sigaction(signal, std::ptr::null(), &mut action) })?;
            if action.sa_sigaction != libc::SIG_DFL {
                return Err(io::Error::other(
                    "shutdown handler replaced the entry policy",
                ));
            }
        }
        for thread in fs::read_dir("/proc/self/task")? {
            verify_thread_mask(fs::read_to_string(thread?.path().join("status")))?;
        }
        Ok(())
    }
}
fn verify_thread_mask(status: io::Result<String>) -> io::Result<()> {
    let status = match status {
        Ok(status) => status,
        // A thread may exit between task enumeration and reading its status.
        // Only procfs's vanished-task errors are safe to ignore.
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) => {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let required = (1u64 << (libc::SIGINT - 1)) | (1u64 << (libc::SIGTERM - 1));
    let blocked = status
        .lines()
        .find_map(|line| line.strip_prefix("SigBlk:"))
        .and_then(|s| u64::from_str_radix(s.trim(), 16).ok());
    if blocked.is_none_or(|bits| bits & required != required) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "every thread must inherit the shutdown-signal mask",
        ));
    }
    Ok(())
}
impl AsRawFd for ShutdownInbox {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.0.as_raw_fd()
    }
}
impl AsFd for ShutdownInbox {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl ShutdownInbox {
    /// Allocation-free, non-consuming final exec check. Errors prohibit exec.
    /// The stopped reader remains owned; pending signals survive a failed exec.
    pub fn is_pending(&self) -> io::Result<bool> {
        let mut fd = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = cvt(unsafe { libc::poll(&mut fd, 1, 0) })?;
        Ok(ready != 0)
    }

    pub fn read_pending(&self) -> io::Result<Vec<ShutdownSignal>> {
        let mut pending = Vec::new();
        // Bound a read even under a continuous signal flood. Standard signals
        // coalesce; any remaining pending signal keeps signalfd readable.
        for _ in 0..2 {
            let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
            let n = unsafe {
                libc::read(
                    self.0.as_raw_fd(),
                    (&mut info as *mut libc::signalfd_siginfo).cast(),
                    std::mem::size_of_val(&info),
                )
            };
            if n < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Ok(pending);
                }
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if n as usize != std::mem::size_of_val(&info) {
                return Err(unsafe_path());
            }
            match info.ssi_signo as i32 {
                libc::SIGINT => pending.push(ShutdownSignal::Interrupt),
                libc::SIGTERM => pending.push(ShutdownSignal::Terminate),
                _ => return Err(unsafe_path()),
            }
        }
        Ok(pending)
    }
}

/// Non-cloneable, owns exactly the two inherited descriptors. Inspection is
/// deliberately separate so it can precede opening even the config directory.
pub struct HandoffEntry {
    lifetime: File,
    ticket: Ticket,
}
impl HandoffEntry {
    /// # Safety
    /// Call at single-threaded process entry. Both descriptors must be exclusively
    /// transferred from the OS (not owned by another Rust object). On error the
    /// caller must terminate, never try ordinary acquire.
    pub unsafe fn inspect(lifetime: i32, ticket: i32) -> io::Result<Self> {
        if fs::read_dir("/proc/self/task")?.count() != 1
            || lifetime < 3
            || ticket < 3
            || lifetime == ticket
        {
            return Err(unsafe_path());
        }
        let fds = fd_snapshot()?;
        if !fds.iter().any(|(fd, _)| *fd == lifetime)
            || !fds.iter().any(|(fd, _)| *fd == ticket)
            || fds
                .iter()
                .any(|(fd, _)| *fd > 2 && *fd != lifetime && *fd != ticket)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unexpected inherited descriptors",
            ));
        }
        // SAFETY: caller transfers exclusive ownership; existence was checked.
        let lifetime = unsafe { File::from_raw_fd(lifetime) };
        let ticket = unsafe { File::from_raw_fd(ticket) };
        set_flags(lifetime.as_raw_fd(), libc::FD_CLOEXEC)?;
        set_flags(ticket.as_raw_fd(), libc::FD_CLOEXEC)?;
        let meta = ticket.metadata()?;
        if !meta.is_file()
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o7777 != 0o600
            || meta.nlink() != 0
            || meta.len() > TICKET_LIMIT
            || cvt(unsafe { libc::fcntl(ticket.as_raw_fd(), libc::F_GET_SEALS) })? & SEALS != SEALS
        {
            return Err(unsafe_path());
        }
        let mut data = vec![0; meta.len() as usize];
        ticket.read_exact_at(&mut data, 0)?;
        let ticket: Ticket = serde_json::from_slice(&data)?;
        ticket.record.validate()?;
        let m = lifetime.metadata()?;
        if ticket.version != 1
            || ticket.pid != std::process::id()
            || ticket.device != m.dev()
            || ticket.inode != m.ino()
        {
            return Err(unsafe_path());
        }
        let mask = mask_now()?;
        // SAFETY: initialized sigset and valid signal numbers.
        if unsafe {
            libc::sigismember(&mask, libc::SIGINT) != 1
                || libc::sigismember(&mask, libc::SIGTERM) != 1
        } {
            return Err(unsafe_path());
        }
        // ticket FD is consumed/closed here; it cannot leak into a second exec.
        Ok(Self { lifetime, ticket })
    }
    pub fn record(&self) -> &HandoffRecord {
        &self.ticket.record
    }
}

impl Instance {
    pub fn config_identity(&self) -> &ConfigIdentity {
        &self.config
    }
    /// Adopt the inherited OFD, without launch acquisition, open, flock or unlock.
    /// All errors are fatal for this entry; there is no cold-start fallback.
    ///
    /// Physical adoption only, before threads. The returned proof must next be
    /// passed to the existing writer's DB Adopt CAS/readback in restricted
    /// bootstrap. No ordinary claim, scan, producer or cold fallback is allowed.
    pub fn adopt_handoff(
        config: &ConfigIdentity,
        ipc_parent: &Path,
        entry: HandoffEntry,
    ) -> io::Result<(Self, InheritedHandoffProof)> {
        Self::adopt_handoff_inner(config, ipc_parent, entry, || Ok(()))
    }

    /// Fixture-only blocking injection inside physical Adopt, before ELF I/O.
    #[cfg(feature = "recovery-test-support")]
    pub fn adopt_handoff_with_physical_hook(
        config: &ConfigIdentity,
        ipc_parent: &Path,
        entry: HandoffEntry,
        before_elf: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<(Self, InheritedHandoffProof)> {
        Self::adopt_handoff_inner(config, ipc_parent, entry, before_elf)
    }
    fn adopt_handoff_inner(
        config: &ConfigIdentity,
        ipc_parent: &Path,
        entry: HandoffEntry,
        before_elf: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<(Self, InheritedHandoffProof)> {
        let HandoffEntry { lifetime, ticket } = entry;
        let record = ticket.record;
        record.validate()?;
        before_elf()?;
        if record.config_identity != config.identity()
            || record.successor_build != running_elf_identity()?
        {
            return Err(unsafe_path());
        }
        regular(&lifetime, &config.lifetime_path())?;
        if !rustix::fs::fcntl_getfl(&lifetime)?.contains(rustix::fs::OFlags::RDWR)
            || !fs::read_to_string(format!("/proc/self/fdinfo/{}", lifetime.as_raw_fd()))?
                .lines()
                .any(|l| l.starts_with("lock:") && l.contains(" FLOCK ") && l.contains(" WRITE "))
        {
            return Err(unsafe_path());
        }
        if fs::read_dir("/proc/self/task")?.count() != 1 {
            return Err(unsafe_path());
        }
        let socket = config.socket(ipc_parent)?;
        reap_socket(&socket)?;
        let proof = InheritedHandoffProof::new(
            record.restart_id,
            record.config_identity,
            record.successor_epoch,
            record.nonce_hash,
            record.successor_build,
        );
        Ok((
            Self {
                identity: config.identity().to_owned(),
                socket,
                config: config.clone(),
                _lifetime: lifetime,
            },
            proof,
        ))
    }

    /// Execute a canonical, freshly validated entrypoint. The launcher supplies
    /// reconstructed effective options; no old launch-FD argument is retained.
    /// Environment and cwd are preserved, including wrapper PATH additions.
    ///
    /// # Safety
    /// The final spawn/FD fence and shutdown-inbox fence must be held. All
    /// threads must inherit the startup SIGINT/SIGTERM mask and never unblock it.
    /// Concurrent FD creation is allowed ONLY with atomic CLOEXEC. Pre-existing
    /// non-CLOEXEC FDs and the stdio/lifetime allowlist must not close/reuse
    /// their numbers until this call returns.
    /// All owned child processes must already be stopped and reaped. On any
    /// returned error keep this Instance alive and enter FailedClosed.
    pub unsafe fn exec_handoff(
        &self,
        signals: &BlockedShutdownSignals,
        path: &Path,
        args: &[OsString],
        record: HandoffRecord,
    ) -> io::Result<Infallible> {
        let prepared = self.prepare_exec_handoff(signals, path, args, record)?;
        // SAFETY: caller holds the same final fences for preparation and exec.
        unsafe { prepared.raw_exec() }
    }
    /// M2 stage 9: all allocation, validation, proc scans, serialization and
    /// ticket I/O finish while the owner heartbeat is still running. No flags
    /// or signal dispositions are changed here.
    pub fn prepare_exec_handoff<'a>(
        &'a self,
        signals: &BlockedShutdownSignals,
        path: &Path,
        args: &[OsString],
        record: HandoffRecord,
    ) -> io::Result<PreparedExec<'a>> {
        record.validate()?;
        if record.config_identity != self.identity {
            return Err(unsafe_path());
        }
        preflight_exec()?;
        signals.verify_masks()?;
        let path = cstring(path.as_os_str())?;
        let mut argv = vec![path.clone()];
        for arg in args {
            if arg.as_os_str().as_bytes().starts_with(b"--launch-fd")
                || arg.as_os_str().as_bytes().starts_with(b"--handoff-")
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "stale handoff/launch argument",
                ));
            }
            argv.push(cstring(arg)?);
        }
        let env = std::env::vars_os()
            .map(|(k, v)| {
                let mut s = k;
                s.push("=");
                s.push(v);
                cstring(&s)
            })
            .collect::<io::Result<Vec<_>>>()?;
        let meta = self._lifetime.metadata()?;
        let ticket = Ticket {
            version: 1,
            pid: std::process::id(),
            device: meta.dev(),
            inode: meta.ino(),
            record,
        };
        let mut file = sealed_ticket(&ticket)?;
        file.flush()?;
        let lifetime = self._lifetime.as_raw_fd();
        let ticket_fd = file.as_raw_fd();
        argv.extend([
            CString::new("--handoff-lifetime-fd").unwrap(),
            CString::new(lifetime.to_string()).unwrap(),
            CString::new("--handoff-ticket-fd").unwrap(),
            CString::new(ticket_fd.to_string()).unwrap(),
        ]);
        let argv_ptrs = pointers(&argv);
        let env_ptrs = pointers(&env);
        let mut flags = fd_snapshot()?;
        flags.retain(|(fd, flags)| {
            flags & libc::FD_CLOEXEC == 0 || *fd <= 2 || *fd == lifetime || *fd == ticket_fd
        });
        Ok(PreparedExec {
            instance: self,
            path,
            _argv: argv,
            _env: env,
            argv_ptrs,
            env_ptrs,
            ticket: file,
            flags,
        })
    }
}

/// Owned heavy-preparation result, lifetime-bound to the lifetime lock. The
/// backing strings, pointer arrays, sealed ticket and rollback FD inventory
/// cannot disappear before the raw call. Preparing alone grants no exec right.
pub struct PreparedExec<'a> {
    instance: &'a Instance,
    path: CString,
    _argv: Vec<CString>,
    _env: Vec<CString>,
    argv_ptrs: Vec<*const libc::c_char>,
    env_ptrs: Vec<*const libc::c_char>,
    ticket: File,
    flags: Vec<(i32, i32)>,
}
impl PreparedExec<'_> {
    /// Minimal synchronous boundary: no proc scan, allocation, DB, hash, flush
    /// or await. Only the final integrator (D-3) may connect this to M2.
    ///
    /// # Safety
    /// The caller must stop the signal reader, check pending signals and its
    /// exec-prohibition latch immediately before this call. The final spawn/FD
    /// fence must remain held; inventoried descriptors must not close/reuse.
    pub unsafe fn raw_exec(mut self) -> io::Result<Infallible> {
        // SAFETY: same final-fence contract as this method.
        let error = unsafe { self.raw_exec_under_latch() };
        if let Err(restore) = self.restore_failed_exec() {
            return Err(io::Error::other(format!(
                "exec failed: {error}; FD restore failed: {restore}"
            )));
        }
        Err(error)
    }
    /// The syscall-only portion of raw_exec. No allocation, cleanup or formatting,
    /// including on error. The caller MUST release its lifecycle/runtime latches
    /// before calling restore_failed_exec and doing diagnostic recovery.
    ///
    /// # Safety
    /// The same stopped-reader, pending/latch and stable-FD fence requirements as
    /// raw_exec apply. A returned error is not permission to resume business.
    pub unsafe fn raw_exec_under_latch(&mut self) -> io::Error {
        let attempt = || -> io::Result<Infallible> {
            cloexec_all(&self.flags, false)?;
            set_flags(self.instance._lifetime.as_raw_fd(), 0)?;
            set_flags(self.ticket.as_raw_fd(), 0)?;
            for (fd, _) in &self.flags {
                if *fd <= 2 {
                    set_flags(*fd, 0)?;
                }
            }
            // SAFETY: pointer arrays and CString backing were prepared beforehand.
            unsafe {
                libc::execve(
                    self.path.as_ptr(),
                    self.argv_ptrs.as_ptr(),
                    self.env_ptrs.as_ptr(),
                );
            }
            Err(io::Error::last_os_error())
        };
        match attempt() {
            Err(error) => error,
            Ok(never) => match never {},
        }
    }
    /// Failed branch only, outside lifecycle/runtime locks. Restore all stable
    /// inventoried flags, even when an earlier restoration fails.
    pub fn restore_failed_exec(&mut self) -> io::Result<()> {
        let mut failure = None;
        for (fd, flags) in &self.flags {
            if let Err(error) = set_flags(*fd, *flags) {
                failure = Some(error);
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

pub fn running_elf_identity() -> io::Result<String> {
    elf_identity(Path::new("/proc/self/exe"))
}
pub fn elf_identity(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut magic = [0; 4];
    file.read_exact(&mut magic)?;
    if &magic != b"\x7fELF" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not an ELF executable",
        ));
    }
    let mut hash = Sha256::new();
    hash.update(magic);
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("sha256:{:x}", hash.finalize()))
}
/// Preflight the fallback resources without changing flags. Actual CLOEXEC is
/// applied only under the final spawn/FD fence.
pub fn preflight_exec() -> io::Result<()> {
    let check = || {
        fd_snapshot()?;
        let file = seal_bytes(&[])?;
        // Never clear CLOEXEC during preflight: concurrent cold/tool spawns
        // must not inherit even this temporary capability-check descriptor.
        set_flags(file.as_raw_fd(), libc::FD_CLOEXEC)
    };
    check().map_err(|e| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!("cannot guarantee handoff FD inheritance: {e}"),
        )
    })
}
fn sealed_ticket(ticket: &Ticket) -> io::Result<File> {
    let data = serde_json::to_vec(ticket)?;
    if data.len() as u64 > TICKET_LIMIT {
        return Err(unsafe_path());
    }
    seal_bytes(&data)
}
fn seal_bytes(data: &[u8]) -> io::Result<File> {
    let fd = cvt(unsafe {
        libc::memfd_create(
            c"evorch-handoff".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    })?;
    let mut f = unsafe { File::from_raw_fd(fd) };
    f.set_permissions(fs::Permissions::from_mode(0o600))?;
    f.write_all(data)?;
    cvt(unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, SEALS) })?;
    Ok(f)
}
fn cstring(s: &std::ffi::OsStr) -> io::Result<CString> {
    CString::new(s.as_bytes()).map_err(io::Error::other)
}
fn pointers(items: &[CString]) -> Vec<*const libc::c_char> {
    items
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}
fn cvt(value: i32) -> io::Result<i32> {
    if value < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}
fn set_flags(fd: i32, flags: i32) -> io::Result<()> {
    cvt(unsafe { libc::fcntl(fd, libc::F_SETFD, flags) }).map(|_| ())
}
fn fd_snapshot() -> io::Result<Vec<(i32, i32)>> {
    let names: Vec<_> = fs::read_dir("/proc/self/fd")?
        .map(|e| {
            e?.file_name()
                .to_str()
                .and_then(|s| s.parse::<i32>().ok())
                .ok_or_else(unsafe_path)
        })
        .collect::<io::Result<_>>()?;
    let mut out = Vec::new();
    for fd in names {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags >= 0 {
            out.push((fd, flags));
        } else if io::Error::last_os_error().raw_os_error() != Some(libc::EBADF) {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(out)
}
fn cloexec_all(flags: &[(i32, i32)], fallback: bool) -> io::Result<()> {
    if !fallback {
        let result = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                3u32,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            )
        };
        if result == 0 {
            return Ok(());
        }
        if !matches!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ENOSYS | libc::EINVAL | libc::EPERM)
        ) {
            return Err(io::Error::last_os_error());
        }
    }
    // /proc was enumerated under the final fence. New FDs must be atomically
    // CLOEXEC; pre-existing non-CLOEXEC FDs cannot close/reuse their numbers.
    // Only those stable, inheritable descriptors need changing on old kernels.
    for (fd, old) in flags {
        if *fd >= 3 && old & libc::FD_CLOEXEC == 0 {
            set_flags(*fd, *old | libc::FD_CLOEXEC)?;
        }
    }
    Ok(())
}
fn mask_now() -> io::Result<libc::sigset_t> {
    let mut set = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut set) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(set)
}
fn set_mask(mask: &libc::sigset_t) -> io::Result<()> {
    let rc = unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, mask, std::ptr::null_mut()) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn thread_exit_between_enumeration_and_status_read_is_allowed() {
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let worker_barrier = barrier.clone();
        let (ready, wait) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut mask = mask_now().unwrap();
            unsafe {
                libc::sigaddset(&mut mask, libc::SIGINT);
                libc::sigaddset(&mut mask, libc::SIGTERM);
            }
            set_mask(&mask).unwrap();
            ready
                .send(unsafe { libc::syscall(libc::SYS_gettid) }.to_string())
                .unwrap();
            worker_barrier.wait();
        });
        let tid = wait.recv().unwrap();
        let entry = fs::read_dir("/proc/self/task")
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| entry.file_name().to_str() == Some(&tid))
            .unwrap();
        let status = entry.path().join("status");
        verify_thread_mask(fs::read_to_string(&status)).unwrap();
        barrier.wait();
        worker.join().unwrap();
        // Kernel procfs retirement may lag pthread_join: either a final blocked
        // mask or a vanished-task error is valid. The errno cases below ensure
        // the vanished branch is covered independently of that kernel timing.
        verify_thread_mask(fs::read_to_string(&status)).unwrap();
    }
    #[test]
    fn thread_mask_rejects_unknown_masks_and_non_exit_read_errors() {
        for errno in [libc::ENOENT, libc::ESRCH] {
            verify_thread_mask(Err(io::Error::from_raw_os_error(errno))).unwrap();
        }
        for errno in [libc::EACCES, libc::EIO, libc::EBADF] {
            assert_eq!(
                verify_thread_mask(Err(io::Error::from_raw_os_error(errno)))
                    .unwrap_err()
                    .raw_os_error(),
                Some(errno)
            );
        }
        // An unclassified NotFound is not evidence of a vanished procfs task.
        assert!(
            verify_thread_mask(Err(io::Error::new(io::ErrorKind::NotFound, "unknown"))).is_err()
        );
        for status in ["", "SigBlk:\tinvalid\n", "SigBlk:\t0000000000000002\n"] {
            assert_eq!(
                verify_thread_mask(Ok(status.into())).unwrap_err().kind(),
                io::ErrorKind::Unsupported
            );
        }
    }
    #[test]
    fn fallback_marks_all_and_restores_flags() {
        preflight_exec().unwrap();
        // Already-CLOEXEC background descriptors may have closed since snapshot.
        let temporary = File::open("/dev/null").unwrap();
        let closed = temporary.as_raw_fd();
        drop(temporary);
        cloexec_all(&[(closed, libc::FD_CLOEXEC)], true).unwrap();
        // Test only this file's FD, never mutate the test runner's FD table.
        let f = File::open("/dev/null").unwrap();
        let fd = f.as_raw_fd();
        set_flags(fd, 0).unwrap();
        cloexec_all(&[(fd, 0)], true).unwrap();
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, libc::FD_CLOEXEC);
        set_flags(fd, 0).unwrap();
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, 0);
    }
    #[test]
    fn ticket_cannot_be_written_resized_or_unsealed() {
        let t = Ticket {
            version: 1,
            pid: std::process::id(),
            device: 0,
            inode: 0,
            record: HandoffRecord {
                restart_id: "r".into(),
                config_identity: "c".into(),
                predecessor_epoch: 0,
                successor_epoch: 1,
                nonce_hash: "n".into(),
                successor_build: "b".into(),
                boot_deadline: boot::BootDeadline::start().unwrap(),
            },
        };
        let f = sealed_ticket(&t).unwrap();
        assert!(f.write_at(b"x", 0).is_err());
        assert!(f.set_len(0).is_err());
        assert_eq!(
            unsafe { libc::fcntl(f.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_WRITE) },
            -1
        );
    }
    #[test]
    fn physical_adoption_rejects_unlocked_or_wrong_lifetime() {
        use std::os::unix::fs::OpenOptionsExt;
        for wrong_inode in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let config = ConfigIdentity::new(directory.path()).unwrap();
            let path = config.lifetime_path();
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .unwrap();
            let lifetime = if wrong_inode {
                let other = directory.path().join("other");
                crate::lock(&other).unwrap()
            } else {
                file
            };
            let record = HandoffRecord {
                restart_id: "r".into(),
                config_identity: config.identity().into(),
                predecessor_epoch: 0,
                successor_epoch: 1,
                nonce_hash: "n".into(),
                successor_build: running_elf_identity().unwrap(),
                boot_deadline: boot::BootDeadline::start().unwrap(),
            };
            let ticket = Ticket {
                version: 1,
                pid: std::process::id(),
                device: 0,
                inode: 0,
                record,
            };
            let entry = HandoffEntry { lifetime, ticket };
            let result = Instance::adopt_handoff(&config, directory.path(), entry);
            assert!(result.is_err());
        }
    }
}
