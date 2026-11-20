//! Linux-only bounded syscall surface. No logging on the termination path.
use std::{
    ffi::{c_int, c_short, c_void},
    fs::{File, OpenOptions},
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{
            fs::{FileTypeExt, OpenOptionsExt},
            net::UnixStream,
        },
    },
};

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: c_short,
    revents: c_short,
}
// Linux libc sigset_t is 128 opaque bytes (glibc/musl), native-word aligned.
// Only libc sig* functions inspect it; this is NOT the kernel sigset ABI.
#[repr(C)]
struct SigSet([usize; 128 / std::mem::size_of::<usize>()]);
unsafe extern "C" {
    fn poll(fds: *mut PollFd, count: usize, timeout: c_int) -> c_int;
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, len: usize) -> isize;
    fn write(fd: c_int, buf: *const c_void, len: usize) -> isize;
    fn send(fd: c_int, buf: *const c_void, len: usize, flags: c_int) -> isize;
    fn sigemptyset(set: *mut SigSet) -> c_int;
    fn sigaddset(set: *mut SigSet, signal: c_int) -> c_int;
    fn sigismember(set: *const SigSet, signal: c_int) -> c_int;
    fn pthread_sigmask(how: c_int, set: *const SigSet, old: *mut SigSet) -> c_int;
    fn _exit(code: c_int) -> !;
}
const POLLIN: c_short = 1;
const POLL_ERRORS: c_short = 8 | 16 | 32; // ERR/HUP/NVAL are not signals.
const F_DUPFD_CLOEXEC: c_int = 1030;
const MSG_DONTWAIT: c_int = 0x40;
const MSG_NOSIGNAL: c_int = 0x4000;
// Linux libc ABI exceptions; do not assume asm-generic mask/open values.
const MIPS: bool = cfg!(any(
    target_arch = "mips",
    target_arch = "mips32r6",
    target_arch = "mips64",
    target_arch = "mips64r6"
));
const SPARC: bool = cfg!(any(target_arch = "sparc", target_arch = "sparc64"));
const SIG_BLOCK: c_int = if MIPS || SPARC { 1 } else { 0 };
const O_NONBLOCK: c_int = if MIPS {
    128
} else if SPARC {
    0x4000
} else {
    0x800
};
const DIAGNOSTIC: &[u8] = b"evorch-core: planned shutdown exceeded 10s; exiting with code 74\n";
const _: () = assert!(DIAGNOSTIC.len() < 512); // POSIX minimum PIPE_BUF.

pub(super) fn duplicate(fd: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: fcntl validates the integer and returns exclusive fd ownership.
    let new = unsafe { fcntl(fd, F_DUPFD_CLOEXEC, 3) };
    if new < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(new) })
    }
}
pub(super) fn wake_pair() -> io::Result<(UnixStream, UnixStream)> {
    // On Linux std uses atomic SOCK_CLOEXEC. These OFDs are new/exclusive,
    // so setting their status flags cannot change the inbox or stderr.
    let (reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    writer.set_nonblocking(true)?;
    Ok((reader, writer))
}
pub(super) fn wake(fd: RawFd) {
    let byte = [1u8];
    // SAFETY: owned socket, one nonblocking SIGPIPE-free send.
    // EAGAIN means unread notifications already make the reader ready.
    unsafe {
        send(fd, byte.as_ptr().cast(), 1, MSG_DONTWAIT | MSG_NOSIGNAL);
    }
}
pub(super) fn pending(fd: RawFd) -> bool {
    let mut pfd = PollFd {
        fd,
        events: POLLIN,
        revents: 0,
    };
    // SAFETY: initialized pollfd; zero timeout, no signal consumption.
    unsafe {
        poll(&mut pfd, 1, 0) > 0 && pfd.revents & POLLIN != 0 && pfd.revents & POLL_ERRORS == 0
    }
}
pub(super) fn wait(control: RawFd, inbox: Option<RawFd>, timeout: i32) -> (bool, bool) {
    let mut fds = [
        PollFd {
            fd: control,
            events: POLLIN,
            revents: 0,
        },
        PollFd {
            fd: inbox.unwrap_or(-1),
            events: POLLIN,
            revents: 0,
        },
    ];
    // SAFETY: initialized array and owned live descriptors.
    let result = unsafe { poll(fds.as_mut_ptr(), fds.len(), timeout) };
    if result > 0 && fds[0].revents & POLLIN != 0 {
        let mut bytes = [0u8; 128];
        // SAFETY: nonblocking socket and fixed stack buffer, no retries.
        unsafe {
            read(control, bytes.as_mut_ptr().cast(), bytes.len());
        }
    }
    (
        result > 0 && fds[1].revents & POLLIN != 0 && fds[1].revents & POLL_ERRORS == 0,
        result > 0 && fds[1].revents & POLL_ERRORS != 0,
    )
}
pub(super) fn prepare_thread_mask() -> io::Result<()> {
    let mut current = SigSet([0; 128 / std::mem::size_of::<usize>()]);
    // SAFETY: valid opaque libc sigset_t storage, null set queries only.
    let error = unsafe { pthread_sigmask(0, std::ptr::null(), &mut current) };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error));
    }
    if unsafe { sigismember(&current, 2) != 1 || sigismember(&current, 15) != 1 } {
        return Err(io::Error::other(
            "shutdown watchdog requires blocked SIGINT/SIGTERM",
        ));
    }
    let mut pipe = SigSet([0; 128 / std::mem::size_of::<usize>()]);
    // SAFETY: block SIGPIPE only in this watchdog thread, retaining INT/TERM.
    let error = unsafe {
        sigemptyset(&mut pipe);
        sigaddset(&mut pipe, 13);
        pthread_sigmask(SIG_BLOCK, &pipe, std::ptr::null_mut())
    };
    if error == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(error))
    }
}

#[cfg(test)]
pub(super) fn block_test_signals() -> io::Result<()> {
    let mut set = SigSet([0; 128 / std::mem::size_of::<usize>()]);
    // SAFETY: only the synthetic unit-test thread; no private policy token is minted.
    let error = unsafe {
        sigemptyset(&mut set);
        sigaddset(&mut set, 2);
        sigaddset(&mut set, 15);
        pthread_sigmask(SIG_BLOCK, &set, std::ptr::null_mut())
    };
    if error == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(error))
    }
}

pub(super) enum Diagnostic {
    Pipe(File),
    Socket(OwnedFd),
    None,
}
impl Diagnostic {
    pub(super) fn prepare() -> Self {
        let Ok(copy) = duplicate(2) else {
            return Self::None;
        };
        let file = File::from(copy);
        let Ok(metadata) = file.metadata() else {
            return Self::None;
        };
        let kind = metadata.file_type();
        if kind.is_socket() {
            return Self::Socket(file.into());
        }
        if kind.is_fifo() {
            // Independent OFD, not F_SETFL on a dup. std's open adds atomic
            // O_CLOEXEC. O_NONBLOCK follows the platform's Linux libc ABI.
            if let Ok(pipe) = OpenOptions::new()
                .write(true)
                .custom_flags(O_NONBLOCK)
                .open("/proc/self/fd/2")
                && pipe.metadata().is_ok_and(|m| m.file_type().is_fifo())
            {
                return Self::Pipe(pipe);
            }
        }
        Self::None
    }
    pub(super) fn terminate(&self) -> ! {
        // No destructor, error construction, retry, logger, heap or lock.
        // SIGPIPE was blocked before ACK; socket additionally uses NOSIGNAL.
        unsafe {
            match self {
                Self::Pipe(fd) => {
                    write(fd.as_raw_fd(), DIAGNOSTIC.as_ptr().cast(), DIAGNOSTIC.len());
                }
                Self::Socket(fd) => {
                    send(
                        fd.as_raw_fd(),
                        DIAGNOSTIC.as_ptr().cast(),
                        DIAGNOSTIC.len(),
                        MSG_DONTWAIT | MSG_NOSIGNAL,
                    );
                }
                Self::None => {}
            }
            _exit(74)
        }
    }
}
