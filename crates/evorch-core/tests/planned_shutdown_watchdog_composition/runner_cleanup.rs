//! Test-parent signal cleanup only; no product signal policy or mask is changed.
use std::{
    ffi::c_int,
    io,
    os::unix::process::CommandExt,
    process::{Child, Command},
    sync::atomic::{AtomicBool, AtomicI32, Ordering},
};
static CHILD: AtomicI32 = AtomicI32::new(0);
static HANDLING: AtomicBool = AtomicBool::new(false);
unsafe extern "C" {
    fn signal(number: c_int, handler: usize) -> usize;
    fn kill(pid: c_int, signal: c_int) -> c_int;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    fn __errno_location() -> *mut c_int;
    fn _exit(code: c_int) -> !;
}
const TERM: c_int = 15;
const INT: c_int = 2;
const KILL: c_int = 9;
const WNOHANG: c_int = 1;
fn wait(pid: c_int, options: c_int) -> c_int {
    loop {
        let mut status = 0;
        // SAFETY: valid stack status, known direct child (or ECHILD). Retry only
        // EINTR; this blocks in the kernel rather than polling/busy-looping.
        let result = unsafe { waitpid(pid, &mut status, options) };
        if result >= 0 || unsafe { *__errno_location() } != 4 {
            return result;
        }
    }
}
extern "C" fn terminate(number: c_int) {
    if HANDLING.swap(true, Ordering::SeqCst) {
        return;
    }
    let pid = CHILD.load(Ordering::SeqCst);
    if pid > 0 && wait(pid, WNOHANG) == 0 {
        // Only a still-live direct child may be killed. ECHILD never authorizes
        // signalling a potentially reused PID after an ordinary wait/reap.
        unsafe {
            kill(pid, KILL);
        }
        wait(pid, 0);
    }
    // Async-signal-safe syscalls and lock-free atomics only. No Rust Drop,
    // allocator, formatting, logging, or shutdown-mask installation here.
    unsafe {
        _exit(128 + number);
    }
}
pub(super) struct Registration {
    term: usize,
    int: usize,
}
impl Drop for Registration {
    fn drop(&mut self) {
        CHILD.store(0, Ordering::SeqCst);
        unsafe {
            signal(TERM, self.term);
            signal(INT, self.int);
        }
        HANDLING.store(false, Ordering::SeqCst);
    }
}
pub(super) fn spawn(command: &mut Command) -> io::Result<(Child, Registration)> {
    let parent = rustix::process::getpid();
    // SAFETY: dedicated harness main is single-threaded at spawn. The child
    // closure uses only direct, async-signal-safe OS calls before exec.
    unsafe {
        command.pre_exec(move || {
            rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))?;
            if rustix::process::getppid() != Some(parent) {
                return Err(io::Error::from_raw_os_error(10)); // parent died before prctl
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    CHILD.store(child.id() as c_int, Ordering::SeqCst);
    // signal() changes dispositions only; INT/TERM remain unblocked in parent.
    let term = unsafe { signal(TERM, terminate as *const () as usize) };
    let int = unsafe { signal(INT, terminate as *const () as usize) };
    if term == usize::MAX || int == usize::MAX {
        let error = io::Error::last_os_error();
        let _ = child.kill();
        let _ = child.wait();
        CHILD.store(0, Ordering::SeqCst);
        if term != usize::MAX {
            unsafe {
                signal(TERM, term);
            }
        }
        if int != usize::MAX {
            unsafe {
                signal(INT, int);
            }
        }
        return Err(error);
    }
    Ok((child, Registration { term, int }))
}
