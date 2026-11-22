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
//!   run may take; they never decide success. READY has one absolute deadline for
//!   the whole report, and each death observation has one absolute deadline for
//!   all processes. The probe's own 60 s deadline would also end the supervisor
//!   (with a status that looks like any other failure), so a row additionally
//!   checks that it finished before that deadline could have expired.
//! - Every process under observation is held through a pidfd opened while the
//!   target was alive and re-validated against `/proc`. No signal is sent to a
//!   bare numeric PID.
//! - This test process is a child subreaper. Whatever a row leaves behind,
//!   including processes that are not registered yet when the row fails, ends up
//!   as a child of this process, where `Cleanup` kills and reaps it without any
//!   help from the supervisor under test. `Cleanup` is bounded, and `Backstop`
//!   kills whatever registered process is still alive after it, so a failing run
//!   always ends.
use core_launcher::handoff::{HandoffTarget, ProbeLimits};
use std::{
    fs, io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{fs::PermissionsExt, process::ExitStatusExt},
    },
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{io::AsyncReadExt, io::unix::AsyncFd, task::JoinHandle, time::Instant};

/// Upper bound for a failing run: waiting for READY, and observing deaths.
const BOUND: Duration = Duration::from_secs(30);
const BEHAVIORS: [&str; 2] = ["hang", "escape"];
/// `P_PIDFD` of waitid(2) (Linux 5.4).
const P_PIDFD: libc::idtype_t = 3;

/// Tests share one process under `cargo test`. Serialising them keeps the
/// ETXTBSY window of freshly written wrapper scripts closed and makes the
/// "no child left" checks exact.
static SERIAL: Mutex<()> = Mutex::new(());

fn deadline() -> Instant {
    Instant::now() + BOUND
}
fn runtime() -> tokio::runtime::Runtime {
    // This process adopts orphans (see `Cleanup`).
    let rc = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    assert_eq!(
        rc,
        0,
        "PR_SET_CHILD_SUBREAPER: {}",
        io::Error::last_os_error()
    );
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

/// Every process a row registered with `Processes::hold`, as an independent
/// pidfd. Rows run one at a time (`SERIAL`), so one registry serves them all.
type Registered = Vec<(&'static str, OwnedFd)>;
static REGISTRY: Mutex<Registered> = Mutex::new(Vec::new());
static LEFT_ALIVE: AtomicBool = AtomicBool::new(false);
fn registry() -> std::sync::MutexGuard<'static, Registered> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs a row under the cleanup guards. Both live inside the future that
/// `block_on` polls, so they run when the row fails, before the runtime is
/// dropped. The runtime is then shut down in the background: dropping it would
/// wait for a probe future that waits for its supervisor.
fn run(test: impl std::future::Future<Output = ()>) {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    run_locked(test);
}
fn run_locked(test: impl std::future::Future<Output = ()>) {
    registry().clear();
    LEFT_ALIVE.store(false, Ordering::SeqCst);
    let runtime = runtime();
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        runtime.block_on(async {
            let _backstop = Backstop;
            let cleanup = Cleanup;
            test.await;
            cleanup.assert_nothing_left();
        })
    }));
    runtime.shutdown_background();
    if LEFT_ALIVE.load(Ordering::SeqCst) {
        eprintln!("the cleanup left registered processes alive (killed by the backstop)");
        if result.is_ok() {
            panic!("the cleanup left registered processes alive");
        }
    }
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
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
fn pidfd_send(fd: &OwnedFd, signal: i32) -> io::Result<()> {
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0 as libc::c_uint,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
fn pidfd_signal(fd: &OwnedFd, signal: i32) {
    pidfd_send(fd, signal).unwrap_or_else(|e| panic!("pidfd_send_signal({signal}): {e}"));
}
/// Non-blocking: has the process terminated (a zombie counts)?
fn pidfd_exited(fd: &OwnedFd) -> bool {
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let rc = unsafe { libc::poll(&mut poll, 1, 0) };
        if rc >= 0 {
            return rc > 0;
        }
        assert_eq!(
            io::Error::last_os_error().kind(),
            io::ErrorKind::Interrupted
        );
    }
}
/// Reaped: signal 0 still succeeds for a zombie, and fails with ESRCH only once
/// the process was reaped.
fn pidfd_reaped(fd: &OwnedFd) -> bool {
    matches!(pidfd_send(fd, 0), Err(e) if e.raw_os_error() == Some(libc::ESRCH))
}
/// Waits (event-driven) until the pidfd is readable, bounded by `until`.
async fn pidfd_wait_exit(fd: &OwnedFd, until: Instant) -> bool {
    let dup = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    assert!(dup >= 0, "dup pidfd: {}", io::Error::last_os_error());
    let ready = AsyncFd::new(unsafe { OwnedFd::from_raw_fd(dup) }).unwrap();
    matches!(
        tokio::time::timeout_at(until, ready.readable()).await,
        Ok(Ok(_))
    )
}
/// Blocks until the pidfd is readable or `until` passes. The remaining time is
/// recomputed after every interruption.
fn pidfd_wait_exit_blocking(fd: &OwnedFd, until: std::time::Instant) -> bool {
    loop {
        let remaining = until.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return pidfd_exited(fd);
        }
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = remaining.as_millis().min(1000) as i32 + 1;
        let rc = unsafe { libc::poll(&mut poll, 1, millis) };
        if rc > 0 {
            return true;
        }
        if rc < 0 {
            assert_eq!(
                io::Error::last_os_error().kind(),
                io::ErrorKind::Interrupted
            );
        }
    }
}
/// Reaps a terminated child through its pidfd without blocking. `Ok(false)`:
/// not our child any more (somebody else reaped it).
fn pidfd_reap_child(fd: &OwnedFd) -> io::Result<bool> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        let rc = unsafe {
            libc::waitid(
                P_PIDFD,
                fd.as_raw_fd() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG,
            )
        };
        if rc == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::ECHILD) => return Ok(false),
            _ => return Err(error),
        }
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
/// Direct children of any thread of this process (zombies included). Orphans
/// adopted by this subreaper are listed as well.
fn children() -> Vec<i32> {
    let mut children = Vec::new();
    for task in fs::read_dir("/proc/self/task").unwrap() {
        let path = task.unwrap().path().join("children");
        if let Ok(list) = fs::read_to_string(path) {
            children.extend(list.split_whitespace().map(|p| p.parse::<i32>().unwrap()));
        }
    }
    children
}

/// Kills and reaps every child of this process, repeatedly: killing a parent
/// hands its children to this subreaper, which the next pass then finds. The
/// listed number is only a candidate. The pidfd is opened first and the parent
/// is re-read, so a number reused by a stranger is never signalled. The whole
/// cleanup has one absolute deadline: a child that survives SIGKILL is reported
/// and left to `Backstop`, never waited for forever.
struct Cleanup;
impl Cleanup {
    fn assert_nothing_left(&self) {
        let left = children();
        assert!(left.is_empty(), "children left behind: {left:?}");
    }
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let me = std::process::id() as i32;
        let until = std::time::Instant::now() + BOUND;
        loop {
            let candidates = children();
            if candidates.is_empty() {
                return;
            }
            if std::time::Instant::now() >= until {
                eprintln!("cleanup gave up on children {candidates:?}");
                return;
            }
            for pid in candidates {
                let Ok(fd) = pidfd_open(pid) else { continue };
                if proc_ppid(pid).ok() != Some(me) {
                    continue;
                }
                match pidfd_send(&fd, libc::SIGKILL) {
                    Ok(()) => {}
                    Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
                    Err(e) => eprintln!("cleanup: SIGKILL of {pid} failed: {e}"),
                }
                if pidfd_wait_exit_blocking(&fd, until) {
                    let _ = pidfd_reap_child(&fd);
                } else {
                    eprintln!("cleanup: {pid} did not terminate");
                }
            }
            std::thread::yield_now();
        }
    }
}

/// Independent of `Cleanup`: after it, kills every registered process that is
/// still alive, through the pidfd taken at registration, and remembers that it
/// had to.
struct Backstop;
impl Drop for Backstop {
    fn drop(&mut self) {
        for (name, fd) in registry().iter() {
            if !pidfd_exited(fd) {
                eprintln!("backstop: {name} still alive after the cleanup");
                LEFT_ALIVE.store(true, Ordering::SeqCst);
                let _ = pidfd_send(fd, libc::SIGKILL);
            }
        }
    }
}

struct Held {
    name: &'static str,
    fd: OwnedFd,
}
#[derive(Default)]
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
            !pidfd_exited(&fd),
            "{name} died before it could be validated"
        );
        assert_eq!(
            parent, expected_parent,
            "{name} ({pid}) has the wrong owner"
        );
        let again = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        assert!(again >= 0, "dup pidfd: {}", io::Error::last_os_error());
        registry().push((name, unsafe { OwnedFd::from_raw_fd(again) }));
        self.0.push(Held { name, fd });
    }
    fn get(&self, name: &str) -> &OwnedFd {
        &self.0.iter().find(|h| h.name == name).unwrap().fd
    }
    /// Every held process terminates within one absolute deadline.
    async fn assert_all_terminated(&self) {
        let until = deadline();
        for held in &self.0 {
            assert!(
                pidfd_wait_exit(&held.fd, until).await,
                "{} still alive {BOUND:?} after the trigger",
                held.name
            );
        }
    }
    /// Every held process is gone, not merely a zombie. Callers reap the
    /// supervisor first; it reaps its own descendants before it exits.
    fn assert_reaped(&self) {
        for held in &self.0 {
            assert!(pidfd_reaped(&held.fd), "{} was not reaped", held.name);
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
    /// whole tree (including the escaped one) is established. One absolute
    /// deadline covers the connection and the report.
    async fn ready(&self) -> [i32; 4] {
        let until = deadline();
        let (mut stream, _) = tokio::time::timeout_at(until, self.listener.accept())
            .await
            .unwrap_or_else(|_| panic!("READY not seen within {BOUND:?}"))
            .unwrap();
        let mut text = String::new();
        tokio::time::timeout_at(until, stream.read_to_string(&mut text))
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
        let mut held = Processes::default();
        held.hold("supervisor", supervisor, owner);
        held.hold("leader", leader, supervisor);
        held.hold("child", child, leader);
        held.hold("grandchild", grandchild, child);
        held
    }
}

type ProbeTask = JoinHandle<io::Result<HandoffTarget>>;
/// A death that came after the probe's own deadline could have been caused by
/// it, so it proves nothing. Not a success criterion: only a guard against a
/// false positive in a run that was already pathologically slow.
fn assert_before_probe_deadline(started: std::time::Instant) {
    assert!(
        started.elapsed() < limits().timeout,
        "the probe's own deadline may have ended the run"
    );
}
fn limits() -> ProbeLimits {
    // The probe's own deadline is its maximum, far beyond BOUND: a row that
    // ends before BOUND was ended by its trigger, not by the deadline.
    ProbeLimits {
        timeout: Duration::from_secs(60),
        output_bytes: 4096,
    }
}
/// Starts the in-process probe (the test process is the "core") and returns
/// once READY was reported. A probe that ends before READY is a failure.
async fn spawn_probe(
    env: &Env,
    cancel: impl std::future::Future<Output = ()> + Send + 'static,
) -> (ProbeTask, [i32; 4], std::time::Instant) {
    let entry = env.entry.clone();
    let started = std::time::Instant::now();
    let mut task: ProbeTask =
        tokio::spawn(
            async move { HandoffTarget::probe_cancellable(&entry, limits(), cancel).await },
        );
    let pids = tokio::select! {
        pids = env.ready() => pids,
        done = &mut task => panic!("probe ended before READY: {:?}", done.map(|r| r.err())),
    };
    (task, pids, started)
}
async fn start_probe(
    env: &Env,
    cancel: impl std::future::Future<Output = ()> + Send + 'static,
) -> (ProbeTask, Processes, std::time::Instant) {
    let (task, pids, started) = spawn_probe(env, cancel).await;
    let held = env.hold_tree(pids, std::process::id() as i32);
    (task, held, started)
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
    assert_eq!(stat.st_ino, inode, "duplicated the wrong descriptor");
    assert!(
        !pidfd_exited(supervisor) && !pidfd_exited(core),
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
        // Not tokio's Command: no kill-by-number on drop. `Cleanup` owns failures.
        let host = std::process::Command::new(fixture())
            .args(["--probe-host", "--target"])
            .arg(&env.entry)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        // Still unreaped child of this test: its number is ours until wait().
        let host_pid = host.id() as i32;
        let mut core = Processes::default();
        core.hold("core", host_pid, std::process::id() as i32);
        let pids = tokio::select! {
            pids = env.ready() => pids,
            _ = pidfd_wait_exit(core.get("core"), deadline()) => panic!("core ended before READY"),
        };
        let tree = env.hold_tree(pids, host_pid);
        let control = control_stays_open.then(|| {
            hold_control_write_end(core.get("core"), host_pid, tree.get("supervisor"), pids[0])
        });
        // Only the core is killed. The supervisor's owner is that core.
        pidfd_signal(core.get("core"), libc::SIGKILL);
        let status = tokio::task::spawn_blocking(move || {
            let mut host = host;
            host.wait()
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL), "{behavior}");
        tree.assert_all_terminated().await;
        // The orphaned supervisor is ours now (or the dead core's waiter thread
        // reaped it first). It reaps its own descendants before it exits.
        pidfd_reap_child(tree.get("supervisor")).unwrap();
        core.assert_reaped();
        tree.assert_reaped();
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
            let (task, held, started) = start_probe(&env, std::future::pending()).await;
            task.abort();
            held.assert_all_terminated().await;
            assert_before_probe_deadline(started);
            // Joining the aborted task waits for the future's Drop, which joins
            // the supervisor's waiter thread.
            assert!(task.await.err().unwrap().is_cancelled(), "{behavior}");
            held.assert_reaped();
        }
    });
}
#[test]
fn control_eof_on_cancel_stops_supervisor_probe_and_escaped_descendants() {
    run(async {
        for behavior in BEHAVIORS {
            let env = Env::new(behavior);
            let (cancel, cancelled) = tokio::sync::oneshot::channel::<()>();
            let (task, held, started) = start_probe(&env, async move {
                let _ = cancelled.await;
            })
            .await;
            cancel.send(()).unwrap();
            held.assert_all_terminated().await;
            assert_before_probe_deadline(started);
            let error = task.await.unwrap().err().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::Interrupted, "{behavior}");
            held.assert_reaped();
        }
    });
}

/// TERM: SIGTERM sent to the live supervisor through its pidfd. It must kill
/// and reap its tree and the probe must report failure (a TERMed probe is never
/// a successful description). It is not a cancel (that is `Interrupted`), and
/// the elapsed-time guard rules out the deadline.
#[test]
fn term_stops_supervisor_probe_and_escaped_descendants_and_fails_the_probe() {
    run(async {
        for behavior in BEHAVIORS {
            let env = Env::new(behavior);
            let (task, held, started) = start_probe(&env, std::future::pending()).await;
            pidfd_signal(held.get("supervisor"), libc::SIGTERM);
            held.assert_all_terminated().await;
            assert_before_probe_deadline(started);
            let error = task.await.unwrap().err().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::Other, "{behavior}: {error}");
            assert!(
                error.to_string().contains("supervisor status"),
                "{behavior}: not a supervisor failure: {error}"
            );
            held.assert_reaped();
        }
    });
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
            assert!(children().is_empty(), "{name}: stray child before");
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
            assert!(children().is_empty(), "{name}: stray child after");
        }
    });
}

// ---- the cleanup itself ----------------------------------------------------

/// Negative test of the row cleanup. A row fails after the supervisor was
/// stopped (SIGSTOP), so it can neither react to EOF nor reap anything: the
/// rest of the tree is alive, and the failing row itself only holds the
/// supervisor. The cleanup must still kill and reap the whole tree, descendants
/// included. The registry (the independent witnesses) must find nothing alive
/// after it, and every witness must be reaped.
#[test]
fn a_row_failing_midway_leaves_no_descendant_behind() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        run_locked(async {
            let env = Env::new("escape");
            let (_task, pids, _started) = spawn_probe(&env, std::future::pending()).await;
            let me = std::process::id() as i32;
            let tree = env.hold_tree(pids, me);
            let mut held = Processes::default();
            held.hold("supervisor", pids[0], me);
            pidfd_signal(held.get("supervisor"), libc::SIGSTOP);
            drop(tree);
            panic!("simulated failure before the rest of the tree is registered");
        })
    }));
    let message = *result
        .expect_err("the simulated failure must propagate")
        .downcast::<&str>()
        .unwrap();
    assert!(message.starts_with("simulated failure"), "{message}");
    assert!(
        !LEFT_ALIVE.load(Ordering::SeqCst),
        "the cleanup left part of the tree alive"
    );
    let witnesses = registry();
    assert_eq!(witnesses.len(), 5, "the witnesses were not all registered");
    for (name, fd) in witnesses.iter() {
        assert!(pidfd_reaped(fd), "{name} was not reaped by the cleanup");
    }
    assert!(children().is_empty(), "children left behind");
}
