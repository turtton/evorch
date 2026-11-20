//! Single-thread helper reactor. Only this process ever signals a target PGID.
use crate::{
    COMPAT_ID, LaunchSpec, TARGET,
    image::verify_running_identity,
    peer, pidfd_open,
    wire::{self, Decoder, Kind, invalid},
};
use std::{
    ffi::CString,
    io::{self, Read, Write},
    net::Shutdown,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    time::{Duration, Instant},
};

pub fn validate(spec: &LaunchSpec) -> io::Result<()> {
    if spec.argv.is_empty()
        || spec.argv[0].is_empty()
        || spec.cwd.contains(&0)
        || spec
            .env
            .iter()
            .any(|(k, v)| k.is_empty() || k.contains(&0) || k.contains(&b'=') || v.contains(&0))
        || spec.argv.iter().any(|a| a.contains(&0))
    {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    let arg_max = unsafe { libc::sysconf(libc::_SC_ARG_MAX) };
    if arg_max <= 0 {
        return Err(io::Error::last_os_error());
    }
    let strings = spec
        .argv
        .iter()
        .map(|a| a.len() + 1)
        .chain(spec.env.iter().map(|(k, v)| k.len() + v.len() + 2));
    let mut total = (spec.argv.len() + spec.env.len() + 2)
        .checked_mul(std::mem::size_of::<usize>())
        .ok_or_else(|| io::Error::from_raw_os_error(libc::E2BIG))?;
    for n in strings {
        if n > 131072 {
            return Err(io::Error::from_raw_os_error(libc::E2BIG));
        }
        total = total
            .checked_add(n)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::E2BIG))?;
    }
    if total > arg_max as usize {
        return Err(io::Error::from_raw_os_error(libc::E2BIG));
    }
    Ok(())
}
struct Prepared {
    _args: Vec<CString>,
    _env: Vec<CString>,
    args: Vec<*const libc::c_char>,
    env: Vec<*const libc::c_char>,
    paths: Vec<CString>,
    cwd: CString,
}
impl Prepared {
    fn new(spec: LaunchSpec) -> io::Result<Self> {
        validate(&spec)?;
        let args: Vec<CString> = spec
            .argv
            .iter()
            .map(|a| CString::new(a.clone()).unwrap())
            .collect();
        let env: Vec<CString> = spec
            .env
            .iter()
            .map(|(k, v)| {
                let mut b = k.clone();
                b.push(b'=');
                b.extend(v);
                CString::new(b).unwrap()
            })
            .collect();
        let program = &spec.argv[0];
        let paths = if program.contains(&b'/') {
            vec![CString::new(program.clone()).unwrap()]
        } else {
            let path = spec
                .env
                .iter()
                .rev()
                .find(|(k, _)| k == b"PATH")
                .map(|(_, v)| v.as_slice())
                .unwrap_or(b"/bin:/usr/bin");
            path.split(|b| *b == b':')
                .map(|p| {
                    let mut b = p.to_vec();
                    if !b.is_empty() {
                        b.push(b'/');
                    }
                    b.extend(program);
                    CString::new(b).unwrap()
                })
                .collect()
        };
        let mut ap = args.iter().map(|v| v.as_ptr()).collect::<Vec<_>>();
        ap.push(std::ptr::null());
        let mut ep = env.iter().map(|v| v.as_ptr()).collect::<Vec<_>>();
        ep.push(std::ptr::null());
        Ok(Self {
            _args: args,
            _env: env,
            args: ap,
            env: ep,
            paths,
            cwd: CString::new(spec.cwd).unwrap(),
        })
    }
    unsafe fn exec(&self, guard: i32, error: i32) -> ! {
        // The child path performs no allocation, locking, PATH lookup or Rust IO.
        unsafe {
            if libc::syscall(
                libc::SYS_close_range,
                3u32,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            ) != 0
            {
                child_error(error);
            }
            if libc::setsid() < 0 || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0 {
                child_error(error);
            }
            if libc::getppid() != guard {
                libc::_exit(125);
            }
            let mut empty: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut empty);
            if libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) < 0 {
                child_error(error);
            }
            for sig in 1..=64 {
                if sig != libc::SIGKILL && sig != libc::SIGSTOP {
                    libc::signal(sig, libc::SIG_DFL);
                }
            }
            if libc::chdir(self.cwd.as_ptr()) < 0 {
                child_error(error);
            }
            let mut denied = false;
            for path in &self.paths {
                libc::execve(path.as_ptr(), self.args.as_ptr(), self.env.as_ptr());
                match *libc::__errno_location() {
                    libc::EACCES => denied = true,
                    libc::ENOENT | libc::ENOTDIR => {}
                    _ => child_error(error),
                }
            }
            *libc::__errno_location() = if denied { libc::EACCES } else { libc::ENOENT };
            child_error(error)
        }
    }
}
unsafe fn child_error(fd: i32) -> ! {
    unsafe {
        let code = *libc::__errno_location();
        let bytes = code.to_le_bytes();
        libc::write(fd, bytes.as_ptr().cast(), 4);
        libc::_exit(127)
    }
}
fn signals() -> io::Result<OwnedFd> {
    unsafe {
        libc::signal(libc::SIGCHLD, libc::SIG_DFL);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        let mut mask = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
            libc::sigaddset(&mut mask, sig);
        }
        if libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut()) < 0 {
            return Err(io::Error::last_os_error());
        }
        let n = libc::signalfd(-1, &mask, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedFd::from_raw_fd(n))
    }
}
fn connect(name: &str, death: i32, deadline: Instant) -> io::Result<UnixStream> {
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as _;
    if name.len() + 1 > addr.sun_path.len() {
        return Err(invalid("abstract guard address too long"));
    }
    for (to, from) in addr.sun_path[1..].iter_mut().zip(name.bytes()) {
        *to = from as _;
    }
    let len =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + 1 + name.len()) as libc::socklen_t;
    let result = unsafe { libc::connect(fd, (&addr as *const libc::sockaddr_un).cast(), len) };
    if result < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(e);
        }
        let mut fds = [wire::fd(fd, libc::POLLOUT), wire::fd(death, libc::POLLIN)];
        wire::poll(&mut fds, Some(deadline))?;
        if fds[1].revents != 0 {
            return Err(invalid("core died connecting guard"));
        }
        let mut error = 0i32;
        let mut n = 4;
        unsafe {
            if libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&mut error as *mut i32).cast(),
                &mut n,
            ) < 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
    }
    Ok(UnixStream::from(owned))
}
pub fn entry() -> io::Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    if args.len() == 4 && args[1] == "--describe-image" {
        verify_running_identity(
            args[2].parse().map_err(|_| invalid("image dev"))?,
            args[3].parse().map_err(|_| invalid("image inode"))?,
        )?;
        println!(
            "{}",
            serde_json::json!({"guard_abi":wire::ABI,"guard_compat_id":COMPAT_ID,"target":TARGET,"features":[],"compiler":env!("GUARD_COMPILER")})
        );
        return Ok(());
    }
    if args.len() == 2 && args[1] == "--describe" {
        println!(
            "{}",
            serde_json::json!({"guard_abi":wire::ABI,"guard_compat_id":COMPAT_ID,"target":TARGET,"features":[],"compiler":env!("GUARD_COMPILER")})
        );
        return Ok(());
    }
    if args.len() == 3 && ["--verify-bundle", "--smoke-bundle"].contains(&args[1].as_str()) {
        let runtime = crate::GuardRuntime::from_bundle(std::path::Path::new(&args[2]))?;
        if args[1] == "--smoke-bundle" {
            let mut spec = LaunchSpec::new("/proc/self/exe")?;
            spec.argv.push(b"--describe".to_vec());
            let status = runtime
                .spawn(
                    &spec,
                    [
                        std::process::Stdio::null(),
                        std::process::Stdio::null(),
                        std::process::Stdio::null(),
                    ],
                )?
                .wait()?;
            if !status.success() {
                return Err(invalid("guard delivery smoke failed"));
            }
        }
        return Ok(());
    }
    if args.len() != 8 || args[1] != "--launch" {
        return Err(invalid("guard arguments"));
    }
    let parse = |n: usize| {
        args[n]
            .parse::<u64>()
            .map_err(|_| invalid("guard numeric argument"))
    };
    verify_running_identity(parse(2)?, parse(3)?)?;
    let core = i32::try_from(parse(4)?).map_err(|_| invalid("core PID"))?;
    let uid = u32::try_from(parse(5)?).map_err(|_| invalid("core UID"))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let death = pidfd_open(core)?;
    if unsafe { libc::getppid() } != core || unsafe { libc::getuid() } != uid {
        return Err(invalid("core changed before pidfd authentication"));
    }
    let signal = signals()?;
    let mut stream = connect(&args[6], death.as_raw_fd(), deadline)?;
    if !peer(&stream, core, uid)? {
        return Err(invalid("wrong guard controller PID/UID"));
    }
    wire::send(
        &mut stream,
        Kind::Hello,
        format!("{COMPAT_ID}:{}", args[7]).as_bytes(),
        Some(deadline),
    )?;
    let mut decoder = Decoder::default();
    let mut spec = Vec::new();
    loop {
        let (k, p) = wire::receive(&mut stream, &mut decoder, death.as_raw_fd(), Some(deadline))?;
        match k {
            Kind::LaunchChunk => {
                if spec.len() + p.len() > wire::LAUNCH_LIMIT {
                    return Err(invalid("LaunchSpec too large"));
                }
                spec.extend(p);
            }
            Kind::LaunchEnd if p.is_empty() => break,
            _ => return Err(invalid("LaunchSpec expected")),
        }
    }
    let spec: LaunchSpec =
        serde_json::from_slice(&spec).map_err(|_| invalid("LaunchSpec invalid"))?;
    let prepared = match Prepared::new(spec) {
        Ok(p) => p,
        Err(e) => {
            wire::send(
                &mut stream,
                Kind::SpawnError,
                &e.raw_os_error().unwrap_or(libc::EINVAL).to_le_bytes(),
                Some(deadline),
            )?;
            return Ok(());
        }
    };
    wire::send(&mut stream, Kind::Ready, &[], Some(deadline))?;
    let (k, p) = wire::receive(&mut stream, &mut decoder, death.as_raw_fd(), Some(deadline))?;
    if k != Kind::Commit || !p.is_empty() {
        return Err(invalid("Commit expected"));
    }
    let mut pipes = [-1; 2];
    if unsafe { libc::pipe2(pipes.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let error_read = unsafe { OwnedFd::from_raw_fd(pipes[0]) };
    let error_write = unsafe { OwnedFd::from_raw_fd(pipes[1]) };
    let guard = unsafe { libc::getpid() };
    let root = unsafe { libc::fork() };
    if root < 0 {
        wire::send(
            &mut stream,
            Kind::SpawnError,
            &io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EAGAIN)
                .to_le_bytes(),
            None,
        )?;
        return Ok(());
    }
    if root == 0 {
        unsafe { prepared.exec(guard, error_write.as_raw_fd()) }
    }
    drop(error_write);
    for fd in 0..3 {
        unsafe {
            libc::close(fd);
        }
    }
    // All failures after fork must go through the armed lease, never an early ?.
    let mut lease = Lease { root: Some(root) };
    let result = reactor(
        &mut stream,
        &mut decoder,
        &death,
        &signal,
        error_read,
        &mut lease,
    );
    // Cleanup is independent of IPC failure. The Drop fallback also observes
    // WNOWAIT, signals the still-reserved group, retires it, then reaps root.
    if result.is_err() {
        let _ = stream.shutdown(Shutdown::Both);
    }
    drop(lease);
    result
}
struct Lease {
    root: Option<i32>,
}
impl Lease {
    fn signal(&self, sig: i32) -> io::Result<()> {
        if let Some(root) = self.root {
            let n = unsafe { libc::kill(-root, sig) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() != Some(libc::ESRCH) {
                    return Err(e);
                }
            }
        }
        Ok(())
    }
    fn force(&self) -> io::Result<()> {
        if let Some(root) = self.root {
            if unsafe { libc::kill(root, libc::SIGKILL) } < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() != Some(libc::ESRCH) {
                    return Err(e);
                }
            }
            self.signal(libc::SIGKILL)?;
        }
        Ok(())
    }
    fn observe(&self, nohang: bool) -> io::Result<Option<i32>> {
        let Some(root) = self.root else {
            return Err(invalid("retired group observed"));
        };
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        loop {
            let flags = libc::WEXITED | libc::WNOWAIT | if nohang { libc::WNOHANG } else { 0 };
            if unsafe { libc::waitid(libc::P_PID, root as _, &mut info, flags) } == 0 {
                break;
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
        if unsafe { info.si_pid() } == 0 {
            return Ok(None);
        }
        let code = unsafe { info.si_status() };
        Ok(Some(match info.si_code {
            libc::CLD_EXITED => code << 8,
            libc::CLD_KILLED => code,
            libc::CLD_DUMPED => code | 0x80,
            _ => return Err(invalid("unexpected root wait status")),
        }))
    }
    fn finish(&mut self) -> io::Result<i32> {
        // Order is safety-critical: the WNOWAIT anchor is still reserved here.
        self.signal(libc::SIGKILL)?;
        let root = self
            .root
            .take()
            .ok_or_else(|| invalid("double root reap"))?;
        let mut status = 0;
        loop {
            if unsafe { libc::waitpid(root, &mut status, 0) } == root {
                return Ok(status);
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        if self.root.is_some() {
            let _ = self.force();
            if self.observe(false).is_ok() {
                let _ = self.finish();
            }
        }
    }
}
fn enqueue(out: &mut Vec<u8>, kind: Kind, payload: &[u8]) -> io::Result<()> {
    let f = wire::frame(kind, payload)?;
    if out.len() + f.len() > wire::QUEUE_LIMIT {
        return Err(invalid("guard status backpressure overflow"));
    }
    out.extend(f);
    Ok(())
}
fn reactor(
    stream: &mut UnixStream,
    decoder: &mut Decoder,
    death: &OwnedFd,
    signals: &OwnedFd,
    error_read: OwnedFd,
    lease: &mut Lease,
) -> io::Result<()> {
    let rootfd = pidfd_open(lease.root.unwrap())?;
    let mut error = Some(std::fs::File::from(error_read));
    let mut error_bytes = Vec::new();
    let mut exec_sent = false;
    let mut root_status = None;
    let mut reported_root = false;
    let mut waiting = false;
    let mut forced = false;
    let mut connected = true;
    let mut input_closed = false;
    let mut final_sent = false;
    let mut out = Vec::new();
    loop {
        let mut fds = [
            wire::fd(death.as_raw_fd(), libc::POLLIN),
            wire::fd(
                if input_closed && out.is_empty() {
                    -1
                } else {
                    stream.as_raw_fd()
                },
                if connected {
                    (if input_closed { 0 } else { libc::POLLIN })
                        | if out.is_empty() { 0 } else { libc::POLLOUT }
                } else {
                    0
                },
            ),
            wire::fd(signals.as_raw_fd(), libc::POLLIN),
            wire::fd(
                if root_status.is_none() {
                    rootfd.as_raw_fd()
                } else {
                    -1
                },
                libc::POLLIN,
            ),
            wire::fd(error.as_ref().map_or(-1, AsRawFd::as_raw_fd), libc::POLLIN),
        ];
        wire::poll(&mut fds, None)?;
        if fds[0].revents != 0 {
            forced = true;
            connected = false;
            out.clear();
        }
        if connected
            && !input_closed
            && fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
        {
            // Detect EOF even under a full status queue; bounded work per turn.
            let mut consumed = 0;
            while consumed < wire::FRAME_LIMIT {
                match decoder.read(stream) {
                    Ok(0) => {
                        forced = true;
                        input_closed = true;
                        break;
                    }
                    Ok(n) => consumed += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        forced = true;
                        connected = false;
                        break;
                    }
                }
                loop {
                    let frame = match decoder.next_frame() {
                        Ok(f) => f,
                        Err(_) => {
                            forced = true;
                            connected = false;
                            break;
                        }
                    };
                    let Some((k, p)) = frame else {
                        break;
                    };
                    let valid = match k {
                        Kind::Wait if p.is_empty() => {
                            waiting = true;
                            true
                        }
                        Kind::TryReap if p.is_empty() => {
                            if root_status.is_none() {
                                root_status = lease.observe(true)?;
                            }
                            if root_status.is_some() {
                                waiting = true;
                            } else {
                                enqueue(&mut out, Kind::Running, &[])?;
                            }
                            true
                        }
                        Kind::Signal => {
                            let sig = wire::integer(&p)?;
                            if !(1..=64).contains(&sig) {
                                false
                            } else {
                                lease.signal(sig)?;
                                enqueue(&mut out, Kind::Ack, &[])?;
                                true
                            }
                        }
                        _ => false,
                    };
                    if !valid {
                        forced = true;
                        connected = false;
                        break;
                    }
                }
                if !connected {
                    break;
                }
            }
        }
        if fds[2].revents != 0 {
            let mut signal: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::read(
                    signals.as_raw_fd(),
                    (&mut signal as *mut libc::signalfd_siginfo).cast(),
                    std::mem::size_of_val(&signal),
                )
            } > 0
            {
                lease.signal(signal.ssi_signo as i32)?;
            }
        }
        if forced {
            lease.force()?;
        }
        if fds[4].revents != 0
            && let Some(pipe) = error.as_mut()
        {
            let mut b = [0; 4];
            loop {
                match pipe.read(&mut b) {
                    Ok(0) => {
                        error = None;
                        break;
                    }
                    Ok(n) => {
                        error_bytes.extend_from_slice(&b[..n]);
                        if error_bytes.len() > 4 {
                            return Err(invalid("exec errno pipe length"));
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
        }
        if error.is_none() && !exec_sent {
            if error_bytes.is_empty() {
                enqueue(&mut out, Kind::ExecOk, &lease.root.unwrap().to_le_bytes())?;
            } else {
                let code = wire::integer(&error_bytes)?;
                enqueue(&mut out, Kind::SpawnError, &code.to_le_bytes())?;
                forced = true;
                lease.force()?;
            }
            exec_sent = true;
        }
        if root_status.is_none() {
            root_status = lease.observe(true)?;
        }
        if let Some(s) = root_status
            && connected
            && exec_sent
            && !reported_root
        {
            enqueue(&mut out, Kind::RootExited, &s.to_le_bytes())?;
            reported_root = true;
        }
        if root_status.is_some() && (waiting || forced) && !final_sent && exec_sent {
            let status = lease.finish()?;
            if Some(status) != root_status {
                return Err(invalid("root raw status changed after WNOWAIT"));
            }
            if connected {
                enqueue(&mut out, Kind::FinalStatus, &status.to_le_bytes())?;
            }
            final_sent = true;
        }
        if !connected {
            out.clear();
            if final_sent {
                return Ok(());
            }
        }
        if connected && !out.is_empty() {
            match stream.write(&out) {
                Ok(0) => {
                    connected = false;
                    forced = true;
                }
                Ok(n) => {
                    out.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    connected = false;
                    forced = true;
                }
            }
        }
        if final_sent && out.is_empty() {
            return Ok(());
        }
    }
}
