#![cfg(target_os = "linux")]
use process_guard::{GuardChild, GuardRuntime, LaunchSpec, pidfd_open, wire};
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{net::UnixStream, process::ExitStatusExt},
    },
    path::PathBuf,
    process::{Command, Stdio},
};
fn runtime() -> GuardRuntime {
    GuardRuntime::from_bundle(&PathBuf::from(
        std::env::var_os(process_guard::BUNDLE_ENV).expect("prepare bundle"),
    ))
    .unwrap()
}
fn run(rt: &GuardRuntime, script: &str) -> (GuardChild, UnixStream, BufReader<UnixStream>) {
    let (input, child_input) = UnixStream::pair().unwrap();
    let (output, child_output) = UnixStream::pair().unwrap();
    let mut spec = LaunchSpec::new("/bin/sh").unwrap();
    spec.argv
        .extend([b"-c".to_vec(), script.as_bytes().to_vec()]);
    (
        rt.spawn(
            &spec,
            [
                Stdio::from(child_input.into_owned_fd()),
                Stdio::from(child_output.into_owned_fd()),
                Stdio::null(),
            ],
        )
        .unwrap(),
        input,
        BufReader::new(output),
    )
}
trait IntoOwned {
    fn into_owned_fd(self) -> std::os::fd::OwnedFd;
}
impl IntoOwned for UnixStream {
    fn into_owned_fd(self) -> std::os::fd::OwnedFd {
        self.into()
    }
}
fn line(r: &mut impl BufRead) -> String {
    let mut s = String::new();
    assert!(r.read_line(&mut s).unwrap() > 0);
    s.trim().to_owned()
}
fn wait_pid(pid: i32) -> i32 {
    let mut status = 0;
    loop {
        let n = unsafe { libc::waitpid(pid, &mut status, 0) };
        if n == pid {
            return status;
        }
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::Interrupted
        );
    }
}
#[test]
fn status_errno_stdio_and_wait_reservation() {
    let rt = runtime();
    for script in [
        "exit 0",
        "exit 37",
        "kill -TERM $$",
        "kill -KILL $$",
        "ulimit -c 0; kill -ABRT $$",
    ] {
        let expected = Command::new("/bin/sh")
            .args(["-c", script])
            .status()
            .unwrap();
        let (mut child, _, _) = run(&rt, script);
        assert_eq!(
            child.wait().unwrap().into_raw(),
            expected.into_raw(),
            "{script}"
        );
        assert_eq!(child.wait().unwrap().into_raw(), expected.into_raw());
    }
    let temp = tempfile::tempdir().unwrap();
    let noexec = temp.path().join("noexec");
    std::fs::write(&noexec, "x").unwrap();
    let badelf = temp.path().join("badelf");
    std::fs::write(&badelf, "not executable format").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&badelf, std::fs::Permissions::from_mode(0o555)).unwrap();
    for path in [temp.path().join("missing"), noexec, badelf] {
        let expected = Command::new(&path).spawn().unwrap_err().raw_os_error();
        let spec = LaunchSpec::new(path).unwrap();
        let actual = rt
            .spawn(&spec, [Stdio::null(), Stdio::null(), Stdio::null()])
            .unwrap_err()
            .raw_os_error();
        assert_eq!(actual, expected);
    }
    let mut spec = LaunchSpec::new("/bin/sh").unwrap();
    spec.cwd = b"/nonexistent/cwd/guard-test".to_vec();
    assert_eq!(
        rt.spawn(&spec, [Stdio::null(), Stdio::null(), Stdio::null()])
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOENT)
    );
    let (mut child, mut input, mut output) = run(&rt, "echo READY; read gate; exit 37");
    assert_eq!(line(&mut output), "READY");
    assert!(child.try_wait().unwrap().is_none());
    child.request_wait().unwrap();
    input.write_all(b"GO\n").unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(37));
}
#[test]
fn wnowait_anchor_then_group_kill_not_root_exit() {
    // Only the outer test controller adopts fixture descendants. The product
    // guard never becomes subreaper, and waits for exactly its target PID.
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
    let rt = runtime();
    let (mut child, mut input, mut output) = run(
        &rt,
        "(echo DESC:$BASHPID; read gate; echo TAIL) <&0 & echo ROOT:$!:$$; exit 37",
    );
    // /bin/sh may not expose BASHPID; root's $! is the authoritative child PID.
    let mut desc = 0;
    let mut root = 0;
    for _ in 0..2 {
        let text = line(&mut output);
        if let Some(p) = text.strip_prefix("ROOT:") {
            let mut ids = p.split(':');
            desc = ids.next().unwrap().parse::<i32>().unwrap();
            root = ids.next().unwrap().parse::<i32>().unwrap();
        }
    }
    assert_eq!(child.target_id(), root as u32);
    let descfd = pidfd_open(desc).unwrap();
    assert_eq!(child.exited().unwrap().code(), Some(37));
    let stat = std::fs::read_to_string(format!("/proc/{root}/stat")).unwrap();
    assert_eq!(
        stat.rsplit_once(") ").unwrap().1.split_whitespace().next(),
        Some("Z"),
        "leader must still be reserved before wait"
    );
    assert_ne!(unsafe { libc::getpgid(child.id() as i32) }, unsafe {
        libc::getpgid(root)
    });
    // RootExited must NOT kill the descendant: it can still produce output.
    input.write_all(b"GO\n").unwrap();
    assert_eq!(line(&mut output), "TAIL");
    wire::poll(&mut [wire::fd(descfd.as_raw_fd(), libc::POLLIN)], None).unwrap();
    assert!(libc::WIFEXITED(wait_pid(desc)));
    assert_eq!(child.wait().unwrap().code(), Some(37));
    assert!(!std::path::Path::new(&format!("/proc/{root}")).exists());
    let (mut child, _input, mut output) = run(
        &rt,
        "(trap '' TERM INT HUP; echo DESC; read gate) <&0 & echo ROOT:$!; exit 37",
    );
    let mut descendant = 0;
    for _ in 0..2 {
        let s = line(&mut output);
        if let Some(p) = s.strip_prefix("ROOT:") {
            descendant = p.parse().unwrap();
        }
    }
    let fd = pidfd_open(descendant).unwrap();
    assert_eq!(child.exited().unwrap().code(), Some(37));
    assert_eq!(child.wait().unwrap().code(), Some(37));
    wire::poll(&mut [wire::fd(fd.as_raw_fd(), libc::POLLIN)], None).unwrap();
    assert_eq!(libc::WTERMSIG(wait_pid(descendant)), libc::SIGKILL);
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 0) }, 0);
}
#[test]
fn half_close_drop_and_guard_lost_are_not_success() {
    let rt = runtime();
    let (mut child, _input, mut output) = run(&rt, "echo READY; read gate");
    assert_eq!(line(&mut output), "READY");
    child.force_kill().unwrap();
    assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    let (child, _input, mut output) = run(&rt, "echo READY; read gate");
    let target = child.target_id();
    assert_eq!(line(&mut output), "READY");
    drop(child);
    assert!(!std::path::Path::new(&format!("/proc/{target}")).exists());
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
    let (mut child, _input, mut output) = run(&rt, "echo READY; read gate");
    let target = child.target_id() as i32;
    let targetfd = pidfd_open(target).unwrap();
    assert_eq!(line(&mut output), "READY");
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    assert!(child.wait().unwrap_err().to_string().contains("GuardLost"));
    drop(child);
    wire::poll(&mut [wire::fd(targetfd.as_raw_fd(), libc::POLLIN)], None).unwrap();
    assert_eq!(libc::WTERMSIG(wait_pid(target)), libc::SIGKILL);
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 0) }, 0);
}
#[test]
fn target_env_is_not_helper_loader_env() {
    let rt = runtime();
    let (output, child_output) = UnixStream::pair().unwrap();
    let mut spec = LaunchSpec::new("/bin/sh").unwrap();
    spec.argv
        .extend([b"-c".to_vec(), b"printf '%s' \"$GUARD_SENTINEL\"".to_vec()]);
    spec.env = vec![
        (b"GUARD_SENTINEL".to_vec(), vec![b'x', 0xff]),
        (b"LD_PRELOAD".to_vec(), b"/nonexistent/sentinel.so".to_vec()),
        (b"LD_LIBRARY_PATH".to_vec(), b"/nonexistent/libs".to_vec()),
    ];
    let mut child = rt
        .spawn(
            &spec,
            [
                Stdio::null(),
                Stdio::from(child_output.into_owned_fd()),
                Stdio::null(),
            ],
        )
        .unwrap();
    let pid = child.id();
    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).unwrap();
    assert!(!maps.contains("sentinel.so"));
    assert!(child.wait().unwrap().success());
    let mut bytes = Vec::new();
    output.take(1024).read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, vec![b'x', 0xff]);
}

// SCM_RIGHTS keeps the control endpoint alive in the OUTER controller, so
// killing the core cannot accidentally pass these cells by control EOF alone.
fn transfer_fd(socket: &UnixStream, fd: i32) {
    unsafe {
        let mut byte = 0u8;
        let mut iov = libc::iovec {
            iov_base: (&mut byte as *mut u8).cast(),
            iov_len: 1,
        };
        let mut control = [0usize; 8];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = libc::CMSG_SPACE(4) as usize;
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(4) as usize;
        std::ptr::write(libc::CMSG_DATA(c).cast::<i32>(), fd);
        assert_eq!(
            libc::sendmsg(socket.as_raw_fd(), &msg, libc::MSG_NOSIGNAL),
            1
        );
    }
}
fn receive_fd(socket: &UnixStream) -> std::os::fd::OwnedFd {
    use std::os::fd::FromRawFd;
    unsafe {
        let mut byte = 0u8;
        let mut iov = libc::iovec {
            iov_base: (&mut byte as *mut u8).cast(),
            iov_len: 1,
        };
        let mut control = [0usize; 8];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = std::mem::size_of_val(&control);
        assert_eq!(
            libc::recvmsg(socket.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC),
            1
        );
        let c = libc::CMSG_FIRSTHDR(&msg);
        assert!(!c.is_null());
        assert_eq!((*c).cmsg_type, libc::SCM_RIGHTS);
        std::os::fd::OwnedFd::from_raw_fd(std::ptr::read(libc::CMSG_DATA(c).cast::<i32>()))
    }
}
struct FixtureReaper {
    pids: Vec<(i32, std::os::fd::OwnedFd)>,
}
impl FixtureReaper {
    fn new() -> Self {
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        Self { pids: Vec::new() }
    }
    fn add(&mut self, pid: i32) {
        if pid > 0 {
            self.pids.push((pid, pidfd_open(pid).unwrap()));
        }
    }
}
impl Drop for FixtureReaper {
    fn drop(&mut self) {
        for (_, fd) in &self.pids {
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
        }
        for (pid, _) in &self.pids {
            let mut status = 0;
            unsafe {
                libc::waitpid(*pid, &mut status, 0);
            }
        }
        unsafe {
            libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 0);
        }
    }
}
#[test]
fn core_fixture() {
    let Some(mode) = std::env::var_os("GUARD_CORE_FIXTURE") else {
        return;
    };
    let mode = mode.to_str().unwrap();
    let mut controller =
        UnixStream::connect(std::env::var_os("GUARD_CORE_SOCKET").unwrap()).unwrap();
    let rt = runtime();
    if mode == "exec" || mode == "thread" {
        let spawn = move || {
            run(
                &rt,
                "sh -c 'echo DESC:$$; kill -STOP $$' <&0 & echo ROOT:$!:$$; kill -STOP $$",
            )
        };
        let (child, input, mut output) = if mode == "thread" {
            std::thread::spawn(spawn).join().unwrap()
        } else {
            spawn()
        };
        let mut descendant = 0;
        for _ in 0..2 {
            let s = line(&mut output);
            if let Some(p) = s.strip_prefix("ROOT:") {
                descendant = p.split(':').next().unwrap().parse::<i32>().unwrap();
            }
        }
        for pid in [child.id() as i32, child.target_id() as i32, descendant] {
            controller.write_all(&pid.to_le_bytes()).unwrap();
        }
        transfer_fd(&controller, child.control().as_raw_fd());
        let mut gate = [0];
        controller.read_exact(&mut gate).unwrap();
        drop((child, input));
        return;
    }
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener};
    let name = format!("guard-fixture-{}", std::process::id());
    let listener =
        UnixListener::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes()).unwrap()).unwrap();
    let [dev, ino] = rt.image().identity_args();
    let mut helper = Command::new(rt.image().inherited_exec_path())
        .args([
            "--launch",
            &dev,
            &ino,
            &std::process::id().to_string(),
            &unsafe { libc::getuid() }.to_string(),
            &name,
            "public-nonce",
        ])
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let (mut socket, _) = listener.accept().unwrap();
    socket.set_nonblocking(true).unwrap();
    let death = pidfd_open(helper.id() as i32).unwrap();
    let mut decoder = wire::Decoder::default();
    let (kind, _) = wire::receive(&mut socket, &mut decoder, death.as_raw_fd(), None).unwrap();
    assert_eq!(kind, wire::Kind::Hello);
    let mut target = 0;
    if mode != "auth" {
        let mut spec = LaunchSpec::new("/bin/sh").unwrap();
        spec.argv
            .extend([b"-c".to_vec(), b"kill -STOP $$".to_vec()]);
        let data = serde_json::to_vec(&spec).unwrap();
        wire::send(&mut socket, wire::Kind::LaunchChunk, &data, None).unwrap();
        wire::send(&mut socket, wire::Kind::LaunchEnd, &[], None).unwrap();
        assert_eq!(
            wire::receive(&mut socket, &mut decoder, death.as_raw_fd(), None)
                .unwrap()
                .0,
            wire::Kind::Ready
        );
        // Ready cannot have forked any target, even if Commit is withheld.
        assert!(
            std::fs::read_to_string(format!("/proc/{0}/task/{0}/children", helper.id()))
                .unwrap()
                .trim()
                .is_empty()
        );
        if mode == "preexec" {
            assert_eq!(
                unsafe {
                    libc::ptrace(libc::PTRACE_SEIZE, helper.id(), 0, libc::PTRACE_O_TRACEFORK)
                },
                0
            );
            wire::send(&mut socket, wire::Kind::Commit, &[], None).unwrap();
            let mut status = 0;
            assert_eq!(
                unsafe { libc::waitpid(helper.id() as i32, &mut status, libc::__WALL) },
                helper.id() as i32
            );
            assert_eq!((status >> 16) as u32, libc::PTRACE_EVENT_FORK as u32);
            let mut msg = 0u64;
            assert_eq!(
                unsafe { libc::ptrace(libc::PTRACE_GETEVENTMSG, helper.id(), 0, &mut msg) },
                0
            );
            target = msg as i32;
            // Core death also detaches the fixture tracer; guard resumes and
            // must kill the target even though its exec-error pipe was pending.
        }
    }
    for pid in [helper.id() as i32, target, 0] {
        controller.write_all(&pid.to_le_bytes()).unwrap();
    }
    transfer_fd(&controller, socket.as_raw_fd());
    let mut gate = [0];
    controller.read_exact(&mut gate).unwrap();
    let _ = socket.shutdown(std::net::Shutdown::Write);
    let _ = helper.wait();
}
#[test]
fn core_death_auth_ready_preexec_running_and_creator_thread() {
    for mode in ["auth", "ready", "preexec", "exec", "thread"] {
        let mut reaper = FixtureReaper::new();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("controller.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let mut core = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "core_fixture", "--nocapture"])
            .env("GUARD_CORE_FIXTURE", mode)
            .env("GUARD_CORE_SOCKET", &path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        reaper.add(core.id() as i32);
        let (mut socket, _) = listener.accept().unwrap();
        let mut ids = [0u8; 12];
        socket.read_exact(&mut ids).unwrap();
        let pids = ids
            .chunks_exact(4)
            .map(|p| i32::from_le_bytes(p.try_into().unwrap()))
            .collect::<Vec<_>>();
        for pid in &pids {
            reaper.add(*pid);
        }
        let _held_control = receive_fd(&socket);
        core.kill().unwrap();
        assert_eq!(core.wait().unwrap().signal(), Some(libc::SIGKILL));
        // Receipt of pidfd readability is the event, not an elapsed-time guess.
        for (pid, fd) in reaper.pids.iter().skip(1) {
            wire::poll(&mut [wire::fd(fd.as_raw_fd(), libc::POLLIN)], None).unwrap();
            if *pid == pids[1] {
                assert!(
                    !std::path::Path::new(&format!("/proc/{pid}")).exists(),
                    "guard must reap its root"
                );
            } else {
                let mut status = 0;
                assert_eq!(
                    unsafe { libc::waitpid(*pid, &mut status, 0) },
                    *pid,
                    "{mode}: fixture PID must be reaped"
                );
                if *pid != pids[0] {
                    assert_eq!(libc::WTERMSIG(status), libc::SIGKILL, "{mode}");
                }
            }
        }
        let mut any = 0;
        assert_eq!(
            unsafe { libc::waitpid(-1, &mut any, libc::WNOHANG) },
            -1,
            "all fixture processes must be reaped"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
}
#[test]
fn trace_fixture() {
    if let Some(path) = std::env::var_os("GUARD_TRACE_ROOT") {
        let (mut child, _input, _output) = run(&runtime(), "exit 37");
        let root = child.target_id();
        std::fs::write(path, root.to_string()).unwrap();
        assert_eq!(child.exited().unwrap().code(), Some(37));
        assert_eq!(child.wait().unwrap().code(), Some(37));
    }
}
#[test]
fn signal_before_reap_trace_invariant() {
    let temp = tempfile::tempdir().unwrap();
    let trace = temp.path().join("trace");
    let root = temp.path().join("root");
    let status = Command::new("strace")
        .args(["-f", "-e", "trace=kill,wait4,waitid", "-o"])
        .arg(&trace)
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "trace_fixture"])
        .env("GUARD_TRACE_ROOT", &root)
        .status()
        .expect("strace is required for the reservation-order contract");
    assert!(status.success());
    let root = std::fs::read_to_string(root).unwrap();
    let trace = std::fs::read_to_string(trace).unwrap();
    let kill = format!("kill(-{root}, SIGKILL)");
    let reap = format!("wait4({root},");
    let killed = trace
        .find(&kill)
        .expect("guard must signal the reserved group");
    let reaped = trace.find(&reap).expect("guard must reap the target");
    assert!(
        killed < reaped,
        "group kill must precede root reap\n{trace}"
    );
    assert!(
        !trace[reaped..].contains(&kill),
        "no delayed signal may use the retired PGID"
    );
}
