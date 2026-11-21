//! PTY allocation and terminal setup. Descriptor ownership never leaves this module.
use std::{
    fmt,
    fs::File,
    io::{self, Read, Write},
    os::fd::FromRawFd,
    process::Stdio,
};

pub use portable_pty::PtySize;

/// Master-side IO only; this is not a process/group ownership handle.
pub struct PtyMaster {
    master: Box<dyn portable_pty::MasterPty + Send>,
}
impl fmt::Debug for PtyMaster {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtyMaster").finish_non_exhaustive()
    }
}
impl PtyMaster {
    /// EIO from a closed slave is EOF, as in portable-pty.
    pub fn try_clone_reader(&self) -> io::Result<Box<dyn Read + Send>> {
        self.master.try_clone_reader().map_err(pty_error)
    }
    /// May be taken once. Drop sends EOT (EOF in canonical terminal mode).
    pub fn take_writer(&self) -> io::Result<Box<dyn Write + Send>> {
        self.master.take_writer().map_err(pty_error)
    }
    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        self.master.resize(size).map_err(pty_error)
    }
}
fn pty_error(error: impl fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}
pub(crate) fn open(size: PtySize) -> io::Result<(PtyMaster, [Stdio; 3])> {
    let pair = portable_pty::native_pty_system()
        .openpty(size)
        .map_err(pty_error)?;
    let master_fd = pair
        .master
        .as_raw_fd()
        .ok_or_else(|| io::Error::other("PTY master descriptor unavailable"))?;
    // SlavePty intentionally has no descriptor API. Linux TIOCGPTPEER obtains
    // the *same* slave without opening a pathname, and sets CLOEXEC atomically.
    // No fallback on unsupported/denied syscalls (Linux >=5.11 is required).
    let peer = unsafe {
        libc::ioctl(
            master_fd,
            libc::TIOCGPTPEER,
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if peer < 0 {
        return Err(io::Error::last_os_error());
    }
    let slave = crate::non_stdio(unsafe { File::from_raw_fd(peer) })?;
    let stdio = [
        Stdio::from(slave.try_clone()?),
        Stdio::from(slave.try_clone()?),
        Stdio::from(slave),
    ];
    // The core must keep no slave copy, including on handshake/exec failure.
    drop(pair.slave);
    Ok((
        PtyMaster {
            master: pair.master,
        },
        stdio,
    ))
}

/// Called by the authenticated single-thread helper, never in core pre_exec.
pub(crate) fn prepare_helper() -> io::Result<()> {
    if unsafe { libc::setsid() } < 0 || unsafe { libc::ioctl(0, libc::TIOCSCTTY, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Only async-signal-safe operations, before target exec.
///
/// The helper remains session leader in its original PGID. The shell becomes
/// foreground in a distinct PGID in that same session. SIGTTOU must not stop the
/// new background group while it installs itself as foreground.
pub(crate) unsafe fn prepare_target() -> io::Result<()> {
    unsafe {
        if libc::setpgid(0, 0) < 0
            || libc::signal(libc::SIGTTOU, libc::SIG_IGN) == libc::SIG_ERR
            || libc::tcsetpgrp(0, libc::getpid()) < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
