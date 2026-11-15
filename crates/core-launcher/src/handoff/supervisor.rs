//! Private fork-only subreaper. The host is NEVER made a subreaper.
//! Child code uses fixed stack buffers and syscall-only libc calls, no allocator,
//! Rust locks, destructors, logging, or runtime calls after fork.
use super::ProbeLimits;
use std::{
    ffi::CString,
    future::Future,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::Path,
};
use tokio::io::unix::AsyncFd;

pub(super) async fn probe(
    path: &Path,
    limits: ProbeLimits,
    cancel: impl Future<Output = ()>,
) -> io::Result<Vec<u8>> {
    let (output_read, control_write, pid) = start(path, limits)?;
    let mut guard = CancelOnDrop {
        control: Some(control_write),
        pid,
        waiter: None,
    };
    // This thread owns waitpid even if the async operation is dropped. Control
    // EOF asks the supervisor to kill/reap before exiting, never SIGKILL it.
    let (tx, rx) = tokio::sync::oneshot::channel();
    guard.waiter = Some(std::thread::spawn(move || {
        let _ = tx.send(wait(pid));
    }));
    let output = AsyncFd::new(output_read)?;
    let read = async {
        let mut data = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            let mut ready = output.readable().await?;
            let result = ready.try_io(|fd| {
                let n =
                    unsafe { libc::read(fd.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match result {
                Ok(Ok(0)) => return Ok(data),
                Ok(Ok(n)) => {
                    if n > limits.output_bytes.saturating_sub(data.len()) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "probe output limit",
                        ));
                    }
                    data.extend_from_slice(&chunk[..n]);
                }
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => continue,
                Ok(Err(e)) => return Err(e),
                Err(_) => continue,
            }
        }
    };
    let mut completion = Box::pin(rx);
    let mut joined = false;
    let operation = async {
        let data = read.await?;
        let status = (&mut completion).await;
        joined = true;
        let status = status.map_err(io::Error::other)??;
        if status != 0 {
            return Err(io::Error::other(format!(
                "self-description supervisor status {status}"
            )));
        }
        Ok(data)
    };
    let result = tokio::select! {
        result=tokio::time::timeout(limits.timeout,operation)=>result.unwrap_or_else(|_|Err(io::Error::new(io::ErrorKind::TimedOut,"handoff probe timed out"))),
        _=cancel=>Err(io::Error::new(io::ErrorKind::Interrupted,"handoff probe cancelled")),
    };
    guard.control.take();
    if !joined {
        completion.await.map_err(io::Error::other)??;
    }
    result
}
fn start(path: &Path, limits: ProbeLimits) -> io::Result<(OwnedFd, OwnedFd, i32)> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let args = [
        path.as_ptr(),
        c"--describe-handoff".as_ptr(),
        std::ptr::null(),
    ];
    let env: Vec<_> = std::env::vars_os()
        .map(|(mut k, v)| {
            k.push("=");
            k.push(v);
            CString::new(k.as_os_str().as_bytes()).map_err(io::Error::other)
        })
        .collect::<io::Result<_>>()?;
    let envp: Vec<_> = env
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let (control_read, control_write) = pipe()?;
    let (output_read, output_write) = pipe()?;
    // High duplicates avoid dup2 into 3/4 clobbering either source.
    let control = duplicate(&control_read)?;
    let output = duplicate(&output_write)?;
    // Escaped descendants require procfs enumeration; refuse before spawning
    // when that cleanup guarantee is unavailable.
    std::fs::File::open("/proc/thread-self/children").map_err(|e| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!("cannot enumerate probe children: {e}"),
        )
    })?;
    let max_fd = descriptor_limit()?;
    let mut child_action: libc::sigaction = unsafe { std::mem::zeroed() };
    if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut child_action) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if child_action.sa_sigaction == libc::SIG_IGN || child_action.sa_flags & libc::SA_NOCLDWAIT != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "probe supervisor must remain waitable",
        ));
    }
    let parent = unsafe { libc::getpid() };
    let original_mask = unsafe {
        let mut m = std::mem::zeroed();
        let rc = libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut m);
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
        m
    };
    // SAFETY: child takes a non-returning syscall-only branch. All strings and
    // pointer arrays were allocated before fork and remain alive until exec.
    // Block all signals across the raw fork so no inherited host handler can
    // run Rust/allocator code in the syscall-only child.
    let mut all: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigfillset(&mut all);
    }
    let rc = unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &all, std::ptr::null_mut()) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    let pid = unsafe { libc::syscall(libc::SYS_clone, libc::SIGCHLD, 0, 0, 0, 0) as i32 };
    if pid < 0 {
        let error = io::Error::last_os_error();
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &original_mask, std::ptr::null_mut());
        }
        return Err(error);
    }
    if pid == 0 {
        unsafe {
            supervise(ForkParameters {
                parent,
                control: control.as_raw_fd(),
                output: output.as_raw_fd(),
                max_fd,
                path: path.as_ptr(),
                args: args.as_ptr(),
                env: envp.as_ptr(),
                original_mask: &original_mask,
                timeout_ms: limits.timeout.as_millis() as i64,
            })
        }
    }
    let rc =
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &original_mask, std::ptr::null_mut()) };
    if rc != 0 {
        drop(control_write);
        let _ = wait(pid);
        return Err(io::Error::from_raw_os_error(rc));
    }
    drop((control_read, output_write, control, output));
    Ok((output_read, control_write, pid))
}
struct CancelOnDrop {
    control: Option<OwnedFd>,
    pid: i32,
    waiter: Option<std::thread::JoinHandle<()>>,
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.control.take();
        if let Some(waiter) = self.waiter.take() {
            let _ = waiter.join();
        } else {
            let _ = wait(self.pid);
        }
    }
}
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}
fn duplicate(fd: &OwnedFd) -> io::Result<OwnedFd> {
    let n = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(n) })
}
fn descriptor_limit() -> io::Result<i32> {
    let mut limit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if limit.rlim_max > 1_048_576 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cannot bound descriptor cleanup",
        ));
    }
    let mut maximum = limit.rlim_max as i32;
    // Lowering RLIMIT_NOFILE does not close already-open higher descriptors.
    // Include them in the bounded fallback rather than accidentally inheriting
    // a core lock/DB FD on a kernel without close_range.
    for entry in std::fs::read_dir("/proc/self/fd")? {
        let fd: i32 = entry?
            .file_name()
            .to_str()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| io::Error::other("invalid proc descriptor"))?;
        if fd >= 1_048_576 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "descriptor exceeds cleanup bound",
            ));
        }
        maximum = maximum.max(fd + 1);
    }
    Ok(maximum)
}
fn wait(pid: i32) -> io::Result<i32> {
    let mut status = 0;
    loop {
        let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
        if rc == pid {
            return Ok(if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else {
                255
            });
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

struct ForkParameters {
    parent: i32,
    control: i32,
    output: i32,
    max_fd: i32,
    path: *const libc::c_char,
    args: *const *const libc::c_char,
    env: *const *const libc::c_char,
    original_mask: *const libc::sigset_t,
    timeout_ms: i64,
}

// SAFETY throughout: only in a forked process, fixed stack buffers and inherited
// valid pointers. Never return through Rust's forked runtime.
#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn supervise(parameters: ForkParameters) -> ! {
    let ForkParameters {
        parent,
        control,
        output,
        max_fd,
        path,
        args,
        env,
        original_mask,
        timeout_ms,
    } = parameters;
    if libc::dup2(control, 3) < 0 || libc::dup2(output, 4) < 0 {
        libc::_exit(125);
    }
    close_from(5, max_fd);
    for fd in 0..3 {
        libc::close(fd);
    }
    let mut mask: libc::sigset_t = std::mem::zeroed();
    libc::sigemptyset(&mut mask);
    libc::sigaddset(&mut mask, libc::SIGCHLD);
    libc::sigaddset(&mut mask, libc::SIGTERM);
    if libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut()) < 0
        || libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) < 0
        || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) < 0
    {
        libc::_exit(125);
    }
    if libc::getppid() != parent {
        libc::_exit(125);
    }
    let mut action: libc::sigaction = std::mem::zeroed();
    action.sa_sigaction = libc::SIG_DFL;
    libc::sigemptyset(&mut action.sa_mask);
    libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut());
    libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
    let signal_fd = libc::signalfd(-1, &mask, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK);
    if signal_fd < 0 {
        libc::_exit(125);
    }
    let child = libc::syscall(libc::SYS_clone, libc::SIGCHLD, 0, 0, 0, 0) as i32;
    if child < 0 {
        libc::_exit(125);
    }
    if child == 0 {
        if libc::setpgid(0, 0) < 0 {
            libc::_exit(126);
        }
        if libc::dup2(4, 1) < 0 || libc::dup2(4, 2) < 0 {
            libc::_exit(126);
        }
        libc::fcntl(1, libc::F_SETFL, 0); // host enforces the bound; child writes block
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
        if null < 0 || libc::dup2(null, 0) < 0 {
            libc::_exit(126);
        }
        // Reset inherited host handlers before restoring the child's mask.
        for signal in 1..=64 {
            if signal != libc::SIGKILL && signal != libc::SIGSTOP {
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
        close_from(3, max_fd);
        libc::sigprocmask(libc::SIG_SETMASK, original_mask, std::ptr::null_mut());
        libc::execve(path, args, env);
        libc::_exit(127);
    }
    libc::close(4);
    libc::setpgid(child, child);
    let deadline = monotonic_ms().saturating_add(timeout_ms);
    let mut status = 0;
    let mut leader_status = 125;
    let mut done = false;
    let mut poll = [
        libc::pollfd {
            fd: 3,
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        },
        libc::pollfd {
            fd: signal_fd,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    while !done {
        // Observe without reaping: the leader (even a zombie) must pin its
        // PID/PGID until group kill, or that number could target an unrelated
        // new process group after reuse. Cleanup below owns the only reap.
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let rc = libc::waitid(
            libc::P_PID,
            child as u32,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        );
        if rc == 0 && info.si_pid() == child {
            leader_status = if info.si_code == libc::CLD_EXITED {
                info.si_status()
            } else {
                128
            };
            break;
        }
        if rc < 0 && *libc::__errno_location() != libc::EINTR {
            break;
        }
        let remaining = deadline.saturating_sub(monotonic_ms());
        if remaining <= 0 {
            break;
        }
        let polled = libc::poll(poll.as_mut_ptr(), 2, remaining.min(60_000) as i32);
        if polled == 0 {
            break;
        }
        if polled < 0 {
            if *libc::__errno_location() == libc::EINTR {
                continue;
            }
            break;
        }
        if poll[0].revents != 0 {
            done = true;
        }
        if poll[1].revents != 0 {
            let mut info: libc::signalfd_siginfo = std::mem::zeroed();
            // CHLD/TERM coalesce. Bound the drain even if a broken probe
            // continuously forks/exits, so control EOF/deadline stay live.
            for _ in 0..2 {
                if libc::read(
                    signal_fd,
                    (&mut info as *mut libc::signalfd_siginfo).cast(),
                    std::mem::size_of_val(&info),
                ) <= 0
                {
                    break;
                }
                if info.ssi_signo == libc::SIGTERM as u32 {
                    done = true;
                    break;
                }
            }
        }
    }
    libc::kill(-child, libc::SIGKILL);
    // Escaped sessions/groups are reparented to this private subreaper. Each
    // death exposes the next generation; kill owned children and wait again.
    loop {
        kill_children();
        let rc = libc::waitpid(-1, &mut status, 0);
        if rc < 0 && *libc::__errno_location() == libc::ECHILD {
            break;
        }
        if rc < 0 && *libc::__errno_location() != libc::EINTR {
            libc::_exit(125);
        }
    }
    libc::_exit(leader_status);
}
#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn close_from(start: i32, max_fd: i32) {
    if libc::syscall(libc::SYS_close_range, start as u32, u32::MAX, 0u32) == 0 {
        return;
    }
    for fd in start..max_fd {
        libc::close(fd);
    }
}
#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn kill_children() {
    let fd = libc::open(
        c"/proc/thread-self/children".as_ptr(),
        libc::O_RDONLY | libc::O_CLOEXEC,
    );
    if fd < 0 {
        libc::_exit(125);
    }
    let mut buffer = [0u8; 4096];
    let mut pid = 0i32;
    loop {
        let n = libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len());
        if n < 0 {
            if *libc::__errno_location() == libc::EINTR {
                continue;
            }
            libc::_exit(125);
        }
        if n == 0 {
            break;
        }
        for byte in &buffer[..n as usize] {
            if byte.is_ascii_digit() {
                pid = pid.saturating_mul(10).saturating_add((byte - b'0') as i32);
            } else if pid > 0 {
                libc::kill(pid, libc::SIGKILL);
                pid = 0;
            }
        }
    }
    if pid > 0 {
        libc::kill(pid, libc::SIGKILL);
    }
    libc::close(fd);
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn monotonic_ms() -> i64 {
    let mut time: libc::timespec = std::mem::zeroed();
    if libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) < 0 {
        libc::_exit(125);
    }
    time.tv_sec
        .saturating_mul(1000)
        .saturating_add(time.tv_nsec / 1_000_000)
}
