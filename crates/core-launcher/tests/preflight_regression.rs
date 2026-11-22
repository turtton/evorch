#![cfg(all(target_os = "linux", feature = "handoff-test-support"))]
//! B3-06 regression for the private preflight supervisor (D-3a2 B3 plan, B3-06).
//!
//! The supervisor is intentionally *not* a guard-owned process: it keeps its own
//! fork-only subreaper protocol. These tests pin the existing guarantees when the
//! caller ("core") dies, the control pipe reaches EOF, the supervisor receives
//! SIGTERM, and the probe's `execve` fails. Every row also observes the escaped
//! (new session) descendants, which only the subreaper can reach.
//!
//! Test rules this file follows:
//! - Real time only. Real child I/O is never mixed with a paused/advanced clock.
//! - Success is decided by events: READY on the notify socket, and pidfd
//!   readability for death. The 30 s limits below only bound how long a *failing*
//!   run may take (READY wait, death observation); they never decide success.
//! - Every process under observation is held through a pidfd opened while the
//!   target was alive and re-validated against `/proc`. No signal is sent to a
//!   bare numeric PID.
use core_launcher::handoff::{HandoffTarget, ProbeLimits};
use std::{
    fs, io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{fs::PermissionsExt, process::ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::Stdio,
    sync::Mutex,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command, task::JoinHandle};

/// Upper bound for a failing run: waiting for READY, and observing deaths.
const BOUND: Duration = Duration::from_secs(30);
const BEHAVIORS: [&str; 2] = ["hang", "escape"];

/// Tests share one process under `cargo test`. Serialising them keeps the
/// ETXTBSY window of freshly written wrapper scripts closed and makes the
/// "no child left" check of the exec-failure rows exact.
static SERIAL: Mutex<()> = Mutex::new(());

fn run(test: impl std::future::Future<Output = ()>) {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(test);
}

fn fixture() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_core-handoff-fixture")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let test = std::env::current_exe().unwrap();
            test.parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("core-handoff-fixture")
        })
}
fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}
fn write_executable(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

// ---- pidfd ownership -------------------------------------------------------

fn pidfd_open(pid: i32) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0 as libc::c_uint) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}
fn pidfd_signal(fd: &OwnedFd, signal: i32) {
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0 as libc::c_uint,
        )
    };
    assert_eq!(rc, 0, "pidfd_send_signal: {}", io::Error::last_os_error());
}
/// Block until the pidfd is readable (the process terminated) or `millis` pass.
fn pidfd_terminated(fd: i32, millis: i32) -> bool {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let rc = unsafe { libc::poll(&mut poll, 1, millis) };
        if rc >= 0 {
            return rc > 0;
        }
        assert_eq!(
            io::Error::last_os_error().kind(),
            io::ErrorKind::Interrupted
        );
    }
}
fn proc_ppid(pid: i32) -> io::Result<i32> {
    let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("PPid:"))
        .and_then(|v| v.trim().parse().ok())
        .ok_or_else(|| io::Error::other("no PPid"))
}

struct Held {
    name: &'static str,
    fd: OwnedFd,
}
/// Holds pidfds for the processes of one row. On drop (including a failing run)
/// anything still alive is killed through its pidfd, never through a number.
struct Processes(Vec<Held>);
impl Processes {
    /// `pid` was reported by the fixture while it was alive. The pidfd is taken
    /// first, then `/proc` is read and the pidfd must still be alive afterwards:
    /// the parent that was read therefore belongs to the same process. Parentage
    /// proves the reported number is the intended process of this row.
    fn hold(&mut self, name: &'static str, pid: i32, expected_parent: i32) {
        let fd = pidfd_open(pid).unwrap_or_else(|e| panic!("pidfd_open {name} ({pid}): {e}"));
        let parent = proc_ppid(pid).unwrap_or_else(|e| panic!("/proc of {name} ({pid}): {e}"));
        assert!(
            !pidfd_terminated(fd.as_raw_fd(), 0),
            "{name} died before it could be validated"
        );
        assert_eq!(
            parent, expected_parent,
            "{name} ({pid}) has the wrong owner"
        );
        self.0.push(Held { name, fd });
    }
    fn get(&self, name: &str) -> &OwnedFd {
        &self.0.iter().find(|h| h.name == name).unwrap().fd
    }
    /// Every held process terminates. Each wait is bounded by `BOUND` in
    /// parallel; reaching the bound is the failure.
    async fn assert_all_terminated(&self) {
        let waits: Vec<_> = self
            .0
            .iter()
            .map(|h| {
                let raw = h.fd.as_raw_fd();
                (
                    h.name,
                    tokio::task::spawn_blocking(move || {
                        pidfd_terminated(raw, BOUND.as_millis() as i32)
                    }),
                )
            })
            .collect();
        for (name, wait) in waits {
            assert!(
                wait.await.unwrap(),
                "{name} still alive {BOUND:?} after the trigger"
            );
        }
    }
}
impl Drop for Processes {
    fn drop(&mut self) {
        for held in &self.0 {
            if !pidfd_terminated(held.fd.as_raw_fd(), 0) {
                let _ = unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        held.fd.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0 as libc::c_uint,
                    )
                };
            }
        }
    }
}

// ---- fixture wiring --------------------------------------------------------

/// Probe entry point plus the READY notify socket. The entry is a POSIX `sh`
/// wrapper (no bashisms: CI's `sh` is dash) that execs the fixture in the
/// requested `--describe-handoff` behaviour.
struct Env {
    _dist: tempfile::TempDir,
    _sockets: test_socket_dir::SocketDir,
    listener: tokio::net::UnixListener,
    entry: PathBuf,
}
impl Env {
    fn new(behavior: &str) -> Self {
        let sockets = test_socket_dir::SocketDir::new();
        let socket = sockets.path().join("n");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let dist = tempfile::tempdir().unwrap();
        let entry = dist.path().join("entry");
        write_executable(
            &entry,
            &format!(
                "#!/bin/sh\nexec {} --probe-behavior {behavior} --notify {} \"$@\"\n",
                quote(&fixture()),
                quote(&socket)
            ),
        );
        Self {
            _dist: dist,
            _sockets: sockets,
            listener,
            entry,
        }
    }
    /// READY: the fixture reports `supervisor leader child grandchild` once the
    /// whole tree (including the escaped one) is established.
    async fn ready(&self) -> [i32; 4] {
        let (mut stream, _) = tokio::time::timeout(BOUND, self.listener.accept())
            .await
            .unwrap_or_else(|_| panic!("READY not seen within {BOUND:?}"))
            .unwrap();
        let mut text = String::new();
        tokio::time::timeout(BOUND, stream.read_to_string(&mut text))
            .await
            .unwrap_or_else(|_| panic!("READY report not completed within {BOUND:?}"))
            .unwrap();
        let pids: Vec<i32> = text
            .split_whitespace()
            .map(|p| p.parse().unwrap())
            .collect();
        pids.try_into()
            .unwrap_or_else(|p| panic!("bad READY {p:?}"))
    }
    fn hold_tree(&self, pids: [i32; 4], owner: i32) -> Processes {
        let [supervisor, leader, child, grandchild] = pids;
        let mut held = Processes(Vec::new());
        held.hold("supervisor", supervisor, owner);
        held.hold("leader", leader, supervisor);
        held.hold("child", child, leader);
        held.hold("grandchild", grandchild, child);
        held
    }
}

type ProbeTask = JoinHandle<io::Result<HandoffTarget>>;
fn limits() -> ProbeLimits {
    // The probe's own deadline is its maximum, far beyond BOUND: a row that
    // ends before BOUND was ended by its trigger, not by the deadline.
    ProbeLimits {
        timeout: Duration::from_secs(60),
        output_bytes: 4096,
    }
}
/// Starts the in-process probe (the test process is the "core") and returns
/// the held tree after READY. A probe that ends before READY is a failure.
async fn start_probe(
    env: &Env,
    cancel: impl std::future::Future<Output = ()> + Send + 'static,
) -> (ProbeTask, Processes) {
    let entry = env.entry.clone();
    let mut task: ProbeTask =
        tokio::spawn(
            async move { HandoffTarget::probe_cancellable(&entry, limits(), cancel).await },
        );
    let pids = tokio::select! {
        pids = env.ready() => pids,
        done = &mut task => panic!("probe ended before READY: {:?}", done.map(|r| r.err())),
    };
    let held = env.hold_tree(pids, std::process::id() as i32);
    (task, held)
}

// ---- rows ------------------------------------------------------------------

/// Keeps the core's control-pipe write end open after the core is gone, so that
/// EOF cannot be what stops the supervisor. The end is duplicated out of the
/// live core through its pidfd; the pipe is identified by the inode of the
/// supervisor's own control read end (fd 3), both read while both are alive.
fn hold_control_write_end(
    core: &OwnedFd,
    core_pid: i32,
    supervisor: &OwnedFd,
    supervisor_pid: i32,
) -> OwnedFd {
    let pipe_of = |link: String| -> u64 {
        let target = fs::read_link(&link).unwrap_or_else(|e| panic!("{link}: {e}"));
        let target = target.to_str().unwrap().to_owned();
        target
            .strip_prefix("pipe:[")
            .and_then(|s| s.strip_suffix(']'))
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("{link} is not a pipe: {target}"))
    };
    let inode = pipe_of(format!("/proc/{supervisor_pid}/fd/3"));
    let mut found = None;
    for entry in fs::read_dir(format!("/proc/{core_pid}/fd")).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        let Ok(target) = fs::read_link(format!("/proc/{core_pid}/fd/{name}")) else {
            continue;
        };
        if target.to_str() == Some(&format!("pipe:[{inode}]")) {
            assert!(
                found.replace(name.parse::<i32>().unwrap()).is_none(),
                "two ends"
            );
        }
    }
    let number = found.expect("the core holds no control write end");
    let fd = unsafe {
        libc::syscall(
            libc::SYS_pidfd_getfd,
            core.as_raw_fd(),
            number,
            0 as libc::c_uint,
        )
    };
    assert!(
        fd >= 0,
        "pidfd_getfd needs ptrace access to the core: {}",
        io::Error::last_os_error()
    );
    let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) }, 0);
    assert_eq!(stat.st_ino as u64, inode, "duplicated the wrong descriptor");
    assert!(
        !pidfd_terminated(supervisor.as_raw_fd(), 0) && !pidfd_terminated(core.as_raw_fd(), 0),
        "the pair changed while the control end was identified"
    );
    fd
}

/// core death: the caller is SIGKILLed with no chance to run any cleanup. The
/// supervisor must stop and reap the probe's group and its escaped descendants
/// by itself. Normally the dying core also closes the control pipe (EOF), so a
/// second row keeps that pipe open to isolate the PDEATHSIG leg.
async fn core_death(control_stays_open: bool) {
    for behavior in BEHAVIORS {
        let env = Env::new(behavior);
        let mut host = Command::new(fixture())
            .args(["--probe-host", "--target"])
            .arg(&env.entry)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        // Still unreaped child of this test: its number is ours until wait().
        let host_pid = host.id().unwrap() as i32;
        let mut held = Processes(Vec::new());
        held.hold("core", host_pid, std::process::id() as i32);
        let pids = tokio::select! {
            pids = env.ready() => pids,
            status = host.wait() => panic!("core ended before READY: {status:?}"),
        };
        let tree = env.hold_tree(pids, host_pid);
        let control = control_stays_open.then(|| {
            hold_control_write_end(held.get("core"), host_pid, tree.get("supervisor"), pids[0])
        });
        // Only the core is killed. The supervisor's owner is that core.
        pidfd_signal(held.get("core"), libc::SIGKILL);
        let status = host.wait().await.unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL), "{behavior}");
        tree.assert_all_terminated().await;
        drop(control);
    }
}
#[test]
fn core_death_stops_supervisor_probe_and_escaped_descendants() {
    run(core_death(false));
}
#[test]
fn core_death_stops_supervisor_even_while_the_control_pipe_stays_open() {
    run(core_death(true));
}

/// EOF: the control pipe reaches EOF either because the probe future is dropped
/// (thread retirement) or because it is cancelled. The supervisor is never
/// signalled; EOF alone must make it kill and reap the whole tree.
#[test]
fn control_eof_on_drop_stops_supervisor_probe_and_escaped_descendants() {
    run(async {
        for behavior in BEHAVIORS {
            let env = Env::new(behavior);
            let (task, held) = start_probe(&env, std::future::pending()).await;
            task.abort();
            held.assert_all_terminated().await;
            assert!(task.await.err().unwrap().is_cancelled(), "{behavior}");
        }
    });
}
#[test]
fn control_eof_on_cancel_stops_supervisor_probe_and_escaped_descendants() {
    run(async {
        for behavior in BEHAVIORS {
            let env = Env::new(behavior);
            let (cancel, cancelled) = tokio::sync::oneshot::channel::<()>();
            let (task, held) = start_probe(&env, async move {
                let _ = cancelled.await;
            })
            .await;
            cancel.send(()).unwrap();
            held.assert_all_terminated().await;
            let error = task.await.unwrap().err().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::Interrupted, "{behavior}");
        }
    });
}

/// TERM: SIGTERM sent to the live supervisor through its pidfd. It must kill
/// and reap its tree and the probe must report failure (a TERMed probe is never
/// a successful description), and not through the deadline or a cancel.
#[test]
fn term_stops_supervisor_probe_and_escaped_descendants_and_fails_the_probe() {
    run(async {
        for behavior in BEHAVIORS {
            let env = Env::new(behavior);
            let (task, held) = start_probe(&env, std::future::pending()).await;
            pidfd_signal(held.get("supervisor"), libc::SIGTERM);
            held.assert_all_terminated().await;
            let error = task.await.unwrap().err().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::Other, "{behavior}: {error}");
            assert!(
                error.to_string().contains("supervisor status"),
                "{behavior}: not a supervisor failure: {error}"
            );
        }
    });
}

/// Direct children of any thread of this process (zombies included).
fn unreaped_children() -> Vec<String> {
    let mut children = Vec::new();
    for task in fs::read_dir("/proc/self/task").unwrap() {
        let path = task.unwrap().path().join("children");
        if let Ok(list) = fs::read_to_string(path) {
            children.extend(list.split_whitespace().map(str::to_owned));
        }
    }
    children
}

/// exec failure: the probe's `execve` fails in the supervisor's child. That is
/// a probe failure with the exec-failure status, never a success with empty
/// output, and neither the supervisor nor the leader stays behind.
#[test]
fn exec_failure_fails_the_probe_and_leaves_no_process() {
    run(async {
        let dist = tempfile::tempdir().unwrap();
        for (name, content) in [
            ("missing-interpreter", "#!/nonexistent/interpreter\n"),
            // Not a script and not an ELF: execve fails with ENOEXEC.
            ("not-executable-format", "not an executable image\n"),
        ] {
            let entry = dist.path().join(name);
            write_executable(&entry, content);
            assert!(unreaped_children().is_empty(), "{name}: stray child before");
            let error = HandoffTarget::probe(&entry, limits())
                .await
                .err()
                .unwrap_or_else(|| panic!("{name}: failed exec reported a description"));
            assert_eq!(error.kind(), io::ErrorKind::Other, "{name}: {error}");
            assert!(
                error.to_string().contains("status 127"),
                "{name}: not the exec failure status: {error}"
            );
            // The probe returns only after its supervisor was reaped.
            assert!(unreaped_children().is_empty(), "{name}: stray child after");
        }
    });
}
