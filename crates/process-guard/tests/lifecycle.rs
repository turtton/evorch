#![cfg(target_os = "linux")]
use std::os::fd::FromRawFd;
fn shell() -> std::ffi::OsString {
    std::env::var_os("EVORCH_TEST_SHELL").unwrap_or_else(|| "/bin/sh".into())
}
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
    let mut spec = LaunchSpec::new(shell()).unwrap();
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
        let expected = Command::new(shell()).args(["-c", script]).status().unwrap();
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
    let mut spec = LaunchSpec::new(shell()).unwrap();
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
    let mut reaper = FixtureReaper::new();
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
    let (mut child, _input, mut output) = run(&rt, "echo READY; read gate");
    let target = child.target_id() as i32;
    reaper.add(target);
    let targetfd = pidfd_open(target).unwrap();
    assert_eq!(line(&mut output), "READY");
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    assert!(child.wait().unwrap_err().to_string().contains("GuardLost"));
    drop(child);
    wire::poll(&mut [wire::fd(targetfd.as_raw_fd(), libc::POLLIN)], None).unwrap();
    assert_eq!(libc::WTERMSIG(wait_pid(target)), libc::SIGKILL);
}
#[test]
fn nonblocking_backpressure_partial_frame_and_guard_term() {
    let rt = runtime();
    let (mut child, _input, mut output) =
        run(&rt, "trap 'echo TERM; exit 42' TERM; echo READY; read gate");
    assert_eq!(line(&mut output), "READY");
    for fd in ["0", "1", "2"] {
        assert!(!std::path::Path::new(&format!("/proc/{}/fd/{fd}", child.id())).exists());
    }
    let mut target_fds = std::fs::read_dir(format!("/proc/{}/fd", child.target_id()))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect::<Vec<_>>();
    target_fds.sort();
    assert_eq!(target_fds, ["0", "1", "2"]);
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    assert_eq!(line(&mut output), "TERM");
    assert_eq!(child.wait().unwrap().code(), Some(42));

    let (mut child, _input, mut output) = run(&rt, "echo READY; read gate");
    assert_eq!(line(&mut output), "READY");
    let request = wire::frame(wire::Kind::Wait, &[]).unwrap();
    child
        .control()
        .try_clone()
        .unwrap()
        .write_all(&request[..7])
        .unwrap();
    child.force_kill().unwrap();
    assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));

    let (mut child, _input, mut output) = run(&rt, "echo READY; read gate");
    assert_eq!(line(&mut output), "READY");
    struct Resume(u32);
    impl Drop for Resume {
        fn drop(&mut self) {
            unsafe {
                libc::kill(self.0 as i32, libc::SIGCONT);
            }
        }
    }
    let resume = Resume(child.id());
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGSTOP) }, 0);
    let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
    assert_eq!(
        unsafe { libc::waitid(libc::P_PID, child.id(), &mut info, libc::WSTOPPED) },
        0
    );
    let bytes = 4096i32;
    assert_eq!(
        unsafe {
            libc::setsockopt(
                child.control().as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&bytes as *const i32).cast(),
                std::mem::size_of_val(&bytes) as _,
            )
        },
        0
    );
    let error = (0..4096)
        .find_map(|_| child.request_signal(libc::SIGWINCH).err())
        .expect("bounded send buffer must reject, never poll a stopped guard");
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    drop(resume);
    assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
}
#[test]
fn target_env_is_not_helper_loader_env() {
    let rt = runtime();
    let (output, child_output) = UnixStream::pair().unwrap();
    let mut spec = LaunchSpec::new(shell()).unwrap();
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
#[test]
fn peer_credentials_and_launch_validation_are_fail_closed() {
    let (s, _) = UnixStream::pair().unwrap();
    let (pid, uid) = unsafe { (libc::getpid(), libc::getuid()) };
    assert!(process_guard::peer(&s, pid, uid).unwrap());
    assert!(!process_guard::peer(&s, pid + 1, uid).unwrap());
    assert!(!process_guard::peer(&s, pid, uid.wrapping_add(1)).unwrap());
    let rt = runtime();
    for (bad, errno) in [
        (b"bad\0arg".to_vec(), libc::EINVAL),
        (vec![b'x'; 131073], libc::E2BIG),
    ] {
        let mut spec = LaunchSpec::new(shell()).unwrap();
        spec.argv.push(bad);
        let error = rt
            .spawn(&spec, std::array::from_fn(|_| Stdio::null()))
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(errno));
        assert!(
            std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
                .unwrap()
                .trim()
                .is_empty()
        );
    }
}
#[test]
fn loader_fixture() {
    let Some(report) = std::env::var_os("GUARD_LOADER_REPORT") else {
        return;
    };
    let (mut child, _input, mut output) = run(&runtime(), "echo READY; read gate");
    assert_eq!(line(&mut output), "READY");
    std::fs::write(
        report,
        serde_json::to_vec(&[std::process::id(), child.id(), child.target_id()]).unwrap(),
    )
    .unwrap();
    child.force_kill().unwrap();
    child.wait().unwrap();
}
#[test]
fn real_preload_constructor_runs_in_target_not_helper() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("preload.c");
    let library = temp.path().join("preload.so");
    let marker = temp.path().join("loaded");
    let report = temp.path().join("report");
    std::fs::write(&source,br#"
#include <stdlib.h>
#include <stdio.h>
#include <unistd.h>
#include <fcntl.h>
__attribute__((constructor)) static void loaded(void) {
  const char *p=getenv("GUARD_PRELOAD_MARKER"); if(!p) return;
  int fd=open(p,O_WRONLY|O_APPEND|O_CREAT,0600);
  if(fd>=0) {char b[32]; int n=snprintf(b,sizeof(b),"%d\n",getpid()); if(write(fd,b,n)!=n) _exit(121); close(fd);}
}
"#).unwrap();
    assert!(
        Command::new(std::env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .args(["-shared", "-fPIC", "-o"])
            .arg(&library)
            .arg(&source)
            .status()
            .unwrap()
            .success()
    );
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "loader_fixture", "--nocapture"])
        .env("LD_PRELOAD", &library)
        .env("LD_LIBRARY_PATH", temp.path())
        .env("GUARD_PRELOAD_MARKER", &marker)
        .env("GUARD_LOADER_REPORT", &report)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ids: Vec<u32> = serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap();
    let loaded = std::fs::read_to_string(marker)
        .unwrap()
        .lines()
        .map(|p| p.parse::<u32>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        loaded,
        vec![ids[0], ids[2]],
        "the inherited loader env must never reach helper {}",
        ids[1]
    );
    for pid in ids {
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }
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
        // An assertion may have interrupted PID registration. Reclaim only
        // this isolated fixture controller's adopted children, using pidfds.
        loop {
            let pid = std::process::id();
            let children = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
                .unwrap_or_default();
            let children = children
                .split_whitespace()
                .map(|p| p.parse::<i32>().unwrap())
                .collect::<Vec<_>>();
            if children.is_empty() {
                break;
            }
            for child in children {
                if let Ok(fd) = pidfd_open(child) {
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
                let mut status = 0;
                unsafe {
                    libc::waitpid(child, &mut status, 0);
                }
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
    let mut helper = helper_command(&rt, &name, unsafe { libc::getuid() })
        .spawn()
        .unwrap();
    let (mut socket, _) = listener.accept().unwrap();
    socket.set_nonblocking(true).unwrap();
    let death = pidfd_open(helper.id() as i32).unwrap();
    let mut decoder = wire::Decoder::default();
    let mut target = 0;
    // auth: accepted transport, but no Hello authentication has completed.
    if mode != "auth" {
        let (kind, _) = wire::receive(&mut socket, &mut decoder, death.as_raw_fd(), None).unwrap();
        assert_eq!(kind, wire::Kind::Hello);
        let mut spec = LaunchSpec::new(shell()).unwrap();
        spec.argv
            .extend([b"-c".to_vec(), b"kill -STOP $$".to_vec()]);
        let data = serde_json::to_vec(&spec).unwrap();
        if mode == "launch" {
            // Incomplete Launch: no Ready, no target and no Commit yet.
            wire::send(&mut socket, wire::Kind::LaunchChunk, &data[..8], None).unwrap();
        } else {
            for chunk in data.chunks(wire::FRAME_LIMIT) {
                wire::send(&mut socket, wire::Kind::LaunchChunk, chunk, None).unwrap();
            }
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
    }
    if target == 0 {
        assert!(
            std::fs::read_to_string(format!("/proc/{0}/task/{0}/children", helper.id()))
                .unwrap()
                .trim()
                .is_empty()
        );
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
    for mode in ["auth", "launch", "ready", "preexec", "exec", "thread"] {
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
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| i32::from_le_bytes(*p))
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

#[test]
fn exec_core_fixture() {
    use std::os::unix::process::CommandExt;
    let Some(stage) = std::env::var_os("GUARD_EXEC_STAGE") else {
        return;
    };
    let mut socket = UnixStream::connect(std::env::var_os("GUARD_EXEC_SOCKET").unwrap()).unwrap();
    socket.write_all(&std::process::id().to_le_bytes()).unwrap();
    if stage == "before" {
        std::thread::spawn(|| {
            let e = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "exec_core_fixture", "--nocapture"])
                .env("GUARD_EXEC_STAGE", "after")
                .exec();
            panic!("non-leader exec failed: {e}");
        })
        .join()
        .unwrap();
        unreachable!();
    }
    let mut gate = [0];
    socket.read_exact(&mut gate).unwrap();
}
#[test]
fn nonleader_exec_keeps_process_pidfd_until_final_exit() {
    let mut reaper = FixtureReaper::new();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("exec.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let mut core = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "exec_core_fixture", "--nocapture"])
        .env("GUARD_EXEC_STAGE", "before")
        .env("GUARD_EXEC_SOCKET", &path)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    reaper.add(core.id() as i32);
    let (mut before, _) = listener.accept().unwrap();
    let mut pid = [0; 4];
    before.read_exact(&mut pid).unwrap();
    assert_eq!(u32::from_le_bytes(pid), core.id());
    let (mut after, _) = listener.accept().unwrap();
    after.read_exact(&mut pid).unwrap();
    assert_eq!(u32::from_le_bytes(pid), core.id());
    let fd = reaper.pids[0].1.as_raw_fd();
    let mut event = wire::fd(fd, libc::POLLIN);
    assert_eq!(
        unsafe { libc::poll(&mut event, 1, 0) },
        0,
        "exec is not process death"
    );
    after.write_all(&[1]).unwrap();
    wire::poll(&mut [wire::fd(fd, libc::POLLIN)], None).unwrap();
    assert!(core.wait().unwrap().success());
}
#[test]
fn guard_lost_never_uses_a_core_pgid_fallback() {
    let mut reaper = FixtureReaper::new();
    let (mut child, _input, mut output) = run(
        &runtime(),
        "sh -c 'echo DESC:$$; kill -STOP $$' & echo ROOT:$!; read gate",
    );
    let mut descendant = 0;
    for _ in 0..2 {
        let text = line(&mut output);
        if let Some(pid) = text.strip_prefix("ROOT:") {
            descendant = pid.parse::<i32>().unwrap();
        }
    }
    assert!(descendant > 0);
    reaper.add(child.target_id() as i32);
    reaper.add(descendant);
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    assert!(child.wait().unwrap_err().to_string().contains("GuardLost"));
    drop(child);
    assert_eq!(libc::WTERMSIG(wait_pid(reaper.pids[0].0)), libc::SIGKILL);
    let mut descendant_event = wire::fd(reaper.pids[1].1.as_raw_fd(), libc::POLLIN);
    assert_eq!(
        unsafe { libc::poll(&mut descendant_event, 1, 0) },
        0,
        "attacked guard: descendant is unconfirmed, not silently killed by core"
    );
    // The outer controller, not the product, owns this intentional-attack cleanup.
    assert_eq!(
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                reaper.pids[1].1.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        },
        0
    );
    assert_eq!(libc::WTERMSIG(wait_pid(descendant)), libc::SIGKILL);
}

fn helper_command(rt: &GuardRuntime, name: &str, uid: u32) -> Command {
    let [dev, ino] = rt.image().identity_args();
    let mut command = Command::new(rt.image().inherited_exec_path());
    command
        .args([
            "--launch",
            &dev,
            &ino,
            &std::process::id().to_string(),
            &uid.to_string(),
            name,
            "public-nonce",
        ])
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}
#[test]
fn wrong_core_uid_is_rejected_before_hello_even_with_known_nonce() {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener};
    let mut reaper = FixtureReaper::new();
    let rt = runtime();
    let name = format!("guard-wrong-peer-{}", std::process::id());
    let listener =
        UnixListener::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes()).unwrap()).unwrap();
    let mut helper = helper_command(&rt, &name, unsafe { libc::getuid() }.wrapping_add(1))
        .spawn()
        .unwrap();
    reaper.add(helper.id() as i32);
    listener.set_nonblocking(true).unwrap();
    let death = pidfd_open(helper.id() as i32).unwrap();
    let mut fds = [
        wire::fd(listener.as_raw_fd(), libc::POLLIN),
        wire::fd(death.as_raw_fd(), libc::POLLIN),
    ];
    wire::poll(&mut fds, None).unwrap();
    if fds[0].revents != 0 {
        let (mut socket, _) = listener.accept().unwrap();
        socket.set_nonblocking(true).unwrap();
        let result = wire::receive(
            &mut socket,
            &mut wire::Decoder::default(),
            death.as_raw_fd(),
            None,
        );
        assert!(
            result.is_err(),
            "wrong UID must never receive Hello: {result:?}"
        );
    } else {
        assert_ne!(
            fds[1].revents, 0,
            "wrong launch UID is rejected even before connect"
        );
    }
    assert_eq!(helper.wait().unwrap().code(), Some(125));
}
#[test]
fn closed_stdio_fixture() {
    if std::env::var_os("GUARD_CLOSED_STDIO_FIXTURE").is_none() {
        return;
    }
    unsafe {
        libc::close(0);
        libc::close(1);
        libc::close(2);
    }
    let rt = runtime();
    let mut spec = LaunchSpec::new(shell()).unwrap();
    spec.argv.extend([b"-c".to_vec(), b"exit 37".to_vec()]);
    let mut child = rt
        .spawn(&spec, [Stdio::null(), Stdio::null(), Stdio::null()])
        .unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(37));
    // Accepted B1 boundary: Rust startup fills absent stdio with /dev/null.
    // Intentional EBADF inheritance is deferred to the B2 stdio API design.
    let mut spec = LaunchSpec::new(shell()).unwrap();
    spec.argv.extend([
        b"-c".to_vec(),
        b"for fd in 0 1 2; do test /proc/self/fd/$fd -ef /dev/null || exit 90; done; exit 37"
            .to_vec(),
    ]);
    let mut child = rt
        .spawn(
            &spec,
            [Stdio::inherit(), Stdio::inherit(), Stdio::inherit()],
        )
        .unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(37));
    for fd in 0..=2 {
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
    }
    // Do not ask the test harness to print to its deliberately closed stdout.
    unsafe { libc::_exit(0) }
}
#[test]
fn closed_stdio_is_sanitized_to_dev_null_by_helper_startup() {
    let mut reaper = FixtureReaper::new();
    let mut core = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "closed_stdio_fixture", "--nocapture"])
        .env("GUARD_CLOSED_STDIO_FIXTURE", "1")
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    reaper.add(core.id() as i32);
    assert_eq!(core.wait().unwrap().code(), Some(0));
}
#[test]
fn slow_chunk_peer_cannot_extend_absolute_handshake_deadline() {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::{
        net::{SocketAddr, UnixListener},
        process::CommandExt,
    };
    let mut reaper = FixtureReaper::new();
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("clock.c");
    let library = temp.path().join("clock.so");
    // A standalone raw-helper fixture only: no product fault switch and no
    // change to the production env_clear launch. Each consumed chunk advances
    // a frozen monotonic clock by one second and emits an event. No sleep or
    // real-time speed assertion. This catches resetting the deadline per frame.
    std::fs::write(
        &source,
        r#"
#define _GNU_SOURCE
#include <dlfcn.h>
#include <unistd.h>
#include <stdlib.h>
#include <time.h>
#include <sys/socket.h>
#include <errno.h>
static long ticks;
static int note=-1;
__attribute__((constructor)) static void init(void) {
    const char *s=getenv("GUARD_FIXTURE_CLOCK_FD"); if(s) note=atoi(s);
}
int clock_gettime(clockid_t id,struct timespec *out) {
    static int (*real)(clockid_t,struct timespec*);
    static struct timespec base;
    if(!real) real=dlsym(RTLD_NEXT,"clock_gettime");
    if(note<0 || id!=CLOCK_MONOTONIC) return real(id,out);
    if(!base.tv_sec && real(id,&base)) return -1;
    *out=base; out->tv_sec+=ticks; return 0;
}
static void consumed(int fd,ssize_t count) {
    int save=errno,kind=0; socklen_t len=sizeof(kind);
    if(note>=0 && count>0 && !getsockopt(fd,SOL_SOCKET,SO_TYPE,&kind,&len) && kind==SOCK_STREAM) {
        ++ticks; if(write(note,"R",1)!=1) _exit(120);
    }
    errno=save;
}
ssize_t read(int fd,void *buf,size_t len) {
    static ssize_t (*real)(int,void*,size_t);
    if(!real) real=dlsym(RTLD_NEXT,"read");
    ssize_t n=real(fd,buf,len); consumed(fd,n); return n;
}
ssize_t recv(int fd,void *buf,size_t len,int flags) {
    static ssize_t (*real)(int,void*,size_t,int);
    if(!real) real=dlsym(RTLD_NEXT,"recv");
    ssize_t n=real(fd,buf,len,flags); consumed(fd,n); return n;
}
"#,
    )
    .unwrap();
    assert!(
        Command::new(std::env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .args(["-shared", "-fPIC", "-o"])
            .arg(&library)
            .arg(&source)
            .arg("-ldl")
            .status()
            .unwrap()
            .success()
    );
    let (mut events, write_events) = UnixStream::pair().unwrap();
    let high = unsafe { libc::fcntl(write_events.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 64) };
    assert!(high >= 64);
    let inherited = unsafe { std::os::fd::OwnedFd::from_raw_fd(high) };
    let rt = runtime();
    let name = format!("guard-slow-{}", std::process::id());
    let listener =
        UnixListener::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes()).unwrap()).unwrap();
    let mut command = helper_command(&rt, &name, unsafe { libc::getuid() });
    command
        .env("LD_PRELOAD", &library)
        .env("GUARD_FIXTURE_CLOCK_FD", high.to_string());
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(high, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut helper = command.spawn().unwrap();
    reaper.add(helper.id() as i32);
    drop((inherited, write_events));
    let (mut socket, _) = listener.accept().unwrap();
    socket.set_nonblocking(true).unwrap();
    let death = pidfd_open(helper.id() as i32).unwrap();
    assert_eq!(
        wire::receive(
            &mut socket,
            &mut wire::Decoder::default(),
            death.as_raw_fd(),
            None
        )
        .unwrap()
        .0,
        wire::Kind::Hello
    );
    let mut consumed = 0;
    for _ in 0..8 {
        if wire::send(&mut socket, wire::Kind::LaunchChunk, b"x", None).is_err() {
            break;
        }
        let mut event = [0];
        if events.read_exact(&mut event).is_err() {
            break;
        }
        assert_eq!(event, [b'R']);
        consumed += 1;
    }
    assert_eq!(
        consumed, 5,
        "the first 5s deadline must not slide with progress"
    );
    assert_eq!(helper.wait().unwrap().code(), Some(125));
}
