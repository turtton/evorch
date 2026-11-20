//! A supervised child process. Every wait is an event: a line on its stdout, its
//! exit notification, or the join of a reader thread. The deadlines below
//! exist only so that a hung child fails the test instead of the runner; no
//! deadline is ever the way two things are ordered.
//!
//! Cleanup does not depend on the test reaching its end: dropping the value
//! kills the child and everything it owns, reaps it and joins the readers.
//!
//! Safety of signals and of ownership
//! ----------------------------------
//! * Every signal to the child goes through a pidfd opened right after the
//!   spawn, while the child could not yet have been reaped. A pidfd names that
//!   one process, so a reused PID is never signalled.
//! * The exit is observed with `waitid(P_PIDFD, WEXITED | WNOWAIT)`: the child
//!   stays a zombie, and its PID (which is also its process-group id) stays
//!   reserved, until the cleanup has killed the group. Only then is it reaped.
//! * Descendants that left the group (a helper calls `setsid`) or were
//!   orphaned before anyone looked at them (they became children of this
//!   process, a subreaper) are still found: a process is owned when it carries
//!   one of this family's unique markers, in its environment, its command line
//!   or its working directory. A process without a marker is never touched.
use super::observe;
use std::{
    collections::{BTreeSet, HashMap},
    io::{BufRead, BufReader, Write},
    os::{
        fd::{AsFd, BorrowedFd, OwnedFd},
        unix::process::{CommandExt, ExitStatusExt},
    },
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// One stdout line and the instant the reader thread received it.
#[derive(Debug, Clone)]
pub struct Line {
    pub at: Instant,
    pub text: String,
}

#[derive(Debug)]
pub enum WaitError {
    /// The child exited (status) before the awaited event; stderr so far.
    Exited(ExitStatus, String),
    /// Deadline of the failure watchdog.
    TimedOut(Duration),
    /// stdout closed without the child's exit being reported yet.
    OutputClosed,
}

#[derive(Debug)]
pub struct Report {
    pub status: ExitStatus,
    pub stderr: String,
    /// Owned processes that were still running when the cleanup began. They
    /// were killed (and reaped when they had become our children); a
    /// non-empty list is a defect of the child, not of the fixture.
    pub orphans: Vec<observe::ProcInfo>,
    /// Reader threads that did not end within the bound because a process
    /// outside this family still holds the pipe. They are detached.
    pub unjoined_readers: usize,
}

/// Name of the environment variable that carries a family's unique token.
pub const OWNER_ENV: &str = "EVORCH_F4P_FAMILY";
/// Default bound for each reader thread's end after everything owned is dead.
const READER_JOIN_BOUND: Duration = Duration::from_secs(10);
const STDERR_LIMIT: usize = 256 * 1024;

pub struct ManagedProcess {
    pid: i32,
    pidfd: OwnedFd,
    lines: mpsc::Receiver<Line>,
    /// Exit notification of the (not yet reaped) child.
    exit: mpsc::Receiver<ExitStatus>,
    reap: Option<mpsc::Sender<()>>,
    reaped: mpsc::Receiver<ExitStatus>,
    status: Option<ExitStatus>,
    stdin: Option<ChildStdin>,
    stderr: Arc<Mutex<Vec<u8>>>,
    readers: Vec<(JoinHandle<()>, mpsc::Receiver<()>)>,
    reader_bound: Duration,
    waiter: Option<JoinHandle<()>>,
    /// Descendants seen at an observation.
    seen: Vec<observe::ProcInfo>,
    leader_start: u64,
    markers: Vec<String>,
    token: String,
}

fn raw_status(status: &rustix::process::WaitIdStatus) -> ExitStatus {
    let raw = if let Some(code) = status.exit_status() {
        (code & 0xff) << 8
    } else {
        let signal = status.terminating_signal().unwrap_or(0) & 0x7f;
        if status.dumped() {
            signal | 0x80
        } else {
            signal
        }
    };
    ExitStatus::from_raw(raw)
}

/// Marks, in this process, the descriptors that were left without
/// close-on-exec (CI runners leave some) so that a spawn does not inherit them.
/// This runs in the parent, where allocation is allowed: each listed number is
/// first looked up by path (`/proc/self/fd/N`, a syscall that takes no
/// descriptor), and only a number that is still open is borrowed. A descriptor
/// that closes after that check makes `fcntl` fail with EBADF, which is ignored;
/// one whose number was reused only gains the close-on-exec flag, which every
/// descriptor of std and tokio has anyway.
fn mark_inherited_cloexec() {
    for fd in observe::open_fds(std::process::id() as i32)
        .unwrap_or_default()
        .into_iter()
        .filter(|f| f.fd > 2 && !f.cloexec)
        .map(|f| f.fd)
    {
        if std::fs::read_link(format!("/proc/self/fd/{fd}")).is_err() {
            continue;
        }
        // SAFETY: the number was just seen open; a concurrent close can only
        // make the call below fail, and the borrow does not outlive it.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let _ = rustix::io::fcntl_setfd(borrowed, rustix::io::FdFlags::CLOEXEC);
    }
}

/// Which step of a spawn fails, to test that a failure takes the child back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpawnFault {
    None,
    Pidfd,
    PidfdClone,
}

thread_local! {
    /// Pid of the child the last failed spawn on this thread took back.
    pub static LAST_ABORTED_SPAWN: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

/// Kills and reaps a child that was started but cannot be supervised.
fn abort_spawn(child: &mut Child) {
    LAST_ABORTED_SPAWN.with(|last| last.set(child.id() as i32));
    if let Some(group) = rustix::process::Pid::from_raw(child.id() as i32) {
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The decision of ownership, from the evidence about one process.
#[derive(Clone, Copy, Debug, Default)]
pub struct Evidence {
    /// Its parent chain reaches this process (directly or by subreaper adoption).
    pub descendant: bool,
    /// It started at or after the family's leader.
    pub started_with_the_family: bool,
    /// Its chain passes through the live leader of another family.
    pub other_family: bool,
    /// It is in the leader's process group.
    pub same_group: bool,
    /// It carries the family's token in its environment.
    pub token: bool,
    /// Its command line or working directory names a family marker, on a path
    /// boundary.
    pub marker: bool,
}
impl Evidence {
    /// Lineage is required; an identity alone (a string in an argument) never
    /// is enough.
    pub fn owns(self) -> bool {
        self.descendant
            && self.started_with_the_family
            && !self.other_family
            && (self.same_group || self.token || self.marker)
    }
}

/// `arg` is `marker` or lies below it; `name=<path>` forms are looked into.
pub fn names_path(arg: &str, marker: &str) -> bool {
    let below = |s: &str| {
        s == marker
            || s.strip_prefix(marker)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    below(arg) || arg.split_once('=').is_some_and(|(_, value)| below(value))
}

/// Leaders of the live families of this process.
static LEADERS: Mutex<BTreeSet<i32>> = Mutex::new(BTreeSet::new());

impl ManagedProcess {
    /// `markers` are strings that are unique to this family (for example its
    /// isolated directory) and that a descendant carries on its command line
    /// or in its working directory even when its environment was cleared.
    /// The command's stdin/stdout/stderr are replaced by pipes; the child
    /// leads its own process group and carries a unique token in its
    /// environment.
    pub fn spawn_with(command: Command, markers: Vec<String>) -> std::io::Result<Self> {
        Self::spawn_faulty(command, markers, SpawnFault::None)
    }

    /// `spawn_with`, failing at `fault` when it is not `None` (tests only).
    pub fn spawn_faulty(
        mut command: Command,
        markers: Vec<String>,
        fault: SpawnFault,
    ) -> std::io::Result<Self> {
        static SUBREAPER: std::sync::Once = std::sync::Once::new();
        SUBREAPER.call_once(|| {
            rustix::process::set_child_subreaper(Some(rustix::process::getpid()))
                .expect("become a child subreaper");
        });
        mark_inherited_cloexec();
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let token = format!(
            "{}-{}-{:x}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        );
        command
            .env(OWNER_ENV, &token)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child: Child = command.spawn()?;
        let pid = child.id() as i32;
        let leader_start = observe::process(pid).map_or(0, |p| p.start_ticks);
        // Everything that can fail after the fork is done before any thread
        // starts, and a failure takes the child back at once.
        let pidfd = match fault {
            SpawnFault::Pidfd => Err(std::io::Error::other("injected: pidfd_open failed")),
            _ => rustix::process::pidfd_open(
                rustix::process::Pid::from_raw(pid).expect("a child pid"),
                rustix::process::PidfdFlags::empty(),
            )
            .map_err(std::io::Error::from),
        };
        let pidfd = match pidfd {
            Ok(fd) => fd,
            Err(error) => {
                abort_spawn(&mut child);
                return Err(error);
            }
        };
        let wait_fd = match fault {
            SpawnFault::PidfdClone => Err(std::io::Error::other("injected: pidfd clone failed")),
            _ => pidfd.try_clone(),
        };
        let wait_fd = match wait_fd {
            Ok(fd) => fd,
            Err(error) => {
                abort_spawn(&mut child);
                return Err(error);
            }
        };
        LEADERS.lock().unwrap().insert(pid);
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr_pipe = child.stderr.take().expect("piped stderr");
        let stdin = child.stdin.take();
        let (line_tx, lines) = mpsc::channel();
        let (exit_tx, exit) = mpsc::channel();
        let (reap_tx, reap_rx) = mpsc::channel::<()>();
        let (reaped_tx, reaped) = mpsc::channel();
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let mut readers = Vec::new();
        {
            let (done_tx, done_rx) = mpsc::channel();
            let handle = std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(text) = line else { break };
                    let line = Line {
                        at: Instant::now(),
                        text,
                    };
                    if line_tx.send(line).is_err() {
                        break;
                    }
                }
                let _ = done_tx.send(());
            });
            readers.push((handle, done_rx));
        }
        {
            let stderr = stderr.clone();
            let (done_tx, done_rx) = mpsc::channel();
            let handle = std::thread::spawn(move || {
                let mut reader = BufReader::new(stderr_pipe);
                let mut chunk = Vec::new();
                loop {
                    chunk.clear();
                    match reader.read_until(b'\n', &mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let mut kept = stderr.lock().unwrap();
                            if kept.len() < STDERR_LIMIT {
                                kept.extend_from_slice(&chunk);
                            }
                        }
                    }
                }
                let _ = done_tx.send(());
            });
            readers.push((handle, done_rx));
        }
        let waiter = {
            std::thread::spawn(move || {
                // Exit without reaping: the zombie keeps pid and group reserved.
                let status = loop {
                    match rustix::process::waitid(
                        rustix::process::WaitId::PidFd(wait_fd.as_fd()),
                        rustix::process::WaitIdOptions::EXITED
                            | rustix::process::WaitIdOptions::NOWAIT,
                    ) {
                        Err(rustix::io::Errno::INTR) => continue,
                        Ok(Some(info)) => break raw_status(&info),
                        other => panic!("waitid on the supervised child: {other:?}"),
                    }
                };
                let _ = exit_tx.send(status);
                // Reaped only when the cleanup says so (or the owner is gone).
                let _ = reap_rx.recv();
                let status = child.wait().expect("reap the supervised child");
                let _ = reaped_tx.send(status);
            })
        };
        Ok(Self {
            pid,
            pidfd,
            lines,
            exit,
            reap: Some(reap_tx),
            reaped,
            status: None,
            stdin,
            stderr,
            readers,
            reader_bound: READER_JOIN_BOUND,
            waiter: Some(waiter),
            seen: Vec::new(),
            leader_start,
            markers,
            token,
        })
    }

    pub fn spawn(command: Command) -> std::io::Result<Self> {
        Self::spawn_with(command, Vec::new())
    }

    /// Changes the bound of the reader join (a failure bound, not a wait).
    pub fn set_reader_bound(&mut self, bound: Duration) {
        self.reader_bound = bound;
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned()
    }

    fn note_exit(&mut self) -> Option<ExitStatus> {
        if self.status.is_none() {
            self.status = self.exit.try_recv().ok();
        }
        self.status
    }

    /// Next stdout line. The child's exit is an event too: it wakes the wait.
    pub fn next_line(&mut self, watchdog: Duration) -> Result<Line, WaitError> {
        let deadline = Instant::now() + watchdog;
        loop {
            // Lines the child wrote before exiting are delivered first.
            match self.lines.try_recv() {
                Ok(line) => return Ok(line),
                Err(mpsc::TryRecvError::Disconnected) => {
                    return match self.wait_exit(Duration::from_secs(5)) {
                        Some(status) => Err(WaitError::Exited(status, self.stderr())),
                        None => Err(WaitError::OutputClosed),
                    };
                }
                Err(mpsc::TryRecvError::Empty) => (),
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(WaitError::TimedOut(watchdog));
            }
            match self.lines.recv_timeout(left) {
                Ok(line) => return Ok(line),
                Err(mpsc::RecvTimeoutError::Timeout) => return Err(WaitError::TimedOut(watchdog)),
                Err(mpsc::RecvTimeoutError::Disconnected) => (),
            }
        }
    }

    /// Reads lines until `select` returns a value; other lines are skipped.
    pub fn wait_for<T>(
        &mut self,
        watchdog: Duration,
        mut select: impl FnMut(&str) -> Option<T>,
    ) -> Result<(T, Instant), WaitError> {
        let deadline = Instant::now() + watchdog;
        loop {
            let line = self.next_line(deadline.saturating_duration_since(Instant::now()))?;
            if let Some(value) = select(&line.text) {
                return Ok((value, line.at));
            }
        }
    }

    pub fn send_line(&mut self, text: &str) -> std::io::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| std::io::Error::other("stdin closed"))?;
        writeln!(stdin, "{text}")?;
        stdin.flush()
    }
    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Exit notification, if it arrives within the watchdog. The child is not
    /// reaped by this: it stays a zombie until `join` or the drop.
    pub fn wait_exit(&mut self, watchdog: Duration) -> Option<ExitStatus> {
        if let Some(status) = self.note_exit() {
            return Some(status);
        }
        self.status = self.exit.recv_timeout(watchdog).ok();
        self.status
    }

    /// Records the current descendants so that cleanup after the child's own
    /// exit still knows what it must not leave behind.
    pub fn remember_descendants(&mut self) -> &[observe::ProcInfo] {
        for p in observe::descendants(self.pid) {
            if !self
                .seen
                .iter()
                .any(|s| s.pid == p.pid && s.start_ticks == p.start_ticks)
            {
                self.seen.push(p);
            }
        }
        &self.seen
    }

    /// Signals the child through its pidfd. False when it is gone.
    pub fn signal(&self, signal: rustix::process::Signal) -> bool {
        rustix::process::pidfd_send_signal(&self.pidfd, signal).is_ok()
    }

    /// SIGTERM, then the exit notification. Past the watchdog everything owned
    /// is killed (the caller then reports that the child ignored SIGTERM).
    pub fn terminate(&mut self, watchdog: Duration) -> Result<ExitStatus, WaitError> {
        self.remember_descendants();
        self.signal(rustix::process::Signal::TERM);
        match self.wait_exit(watchdog) {
            Some(status) => Ok(status),
            None => {
                self.kill_all();
                Err(WaitError::TimedOut(watchdog))
            }
        }
    }

    /// The evidence about one process of the table, `parent_of` being the
    /// parent link of every process. Reads only `/proc`.
    fn evidence(&self, p: &observe::ProcInfo, parent_of: &HashMap<i32, i32>) -> Evidence {
        let me = std::process::id() as i32;
        // Walk up to the process that is a child of this one.
        let mut top = None;
        let mut at = p.pid;
        for _ in 0..4096 {
            match parent_of.get(&at) {
                Some(&parent) if parent == me => {
                    top = Some(at);
                    break;
                }
                Some(&parent) if parent > 1 => at = parent,
                _ => break,
            }
        }
        let token = {
            let needle = format!("{OWNER_ENV}={}", self.token).into_bytes();
            std::fs::read(format!("/proc/{}/environ", p.pid))
                .is_ok_and(|env| env.split(|b| *b == 0).any(|entry| entry == needle))
        };
        let marker = !self.markers.is_empty() && {
            let cwd = std::fs::read_link(format!("/proc/{}/cwd", p.pid))
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.markers.iter().any(|m| {
                names_path(&cwd, m) || observe::cmdline(p.pid).iter().any(|arg| names_path(arg, m))
            })
        };
        Evidence {
            descendant: top.is_some(),
            // The clock has 10 ms ticks: a process that started in the same tick
            // as the leader cannot be told apart from it by time alone.
            started_with_the_family: p.start_ticks >= self.leader_start,
            other_family: top
                .is_some_and(|t| t != self.pid && LEADERS.lock().unwrap().contains(&t)),
            same_group: p.pgid == self.pid,
            token,
            marker,
        }
    }

    /// Everything this family owns right now, except the leader: processes
    /// seen earlier (same incarnation), processes whose evidence says they are
    /// ours, and the descendants of both. A process the evidence does not
    /// clear is never touched, whatever its command line says.
    fn owned_now(&self) -> Vec<observe::ProcInfo> {
        let all = observe::all_processes();
        let parent_of: HashMap<i32, i32> = all.iter().map(|p| (p.pid, p.ppid)).collect();
        let mut found: BTreeSet<i32> = self
            .seen
            .iter()
            .filter(|s| {
                all.iter()
                    .any(|p| p.pid == s.pid && p.start_ticks == s.start_ticks)
            })
            .map(|s| s.pid)
            .collect();
        found.extend(
            all.iter()
                .filter(|p| p.pid != self.pid && self.evidence(p, &parent_of).owns())
                .map(|p| p.pid),
        );
        loop {
            let before = found.len();
            for p in &all {
                if found.contains(&p.ppid) && p.pid != self.pid {
                    found.insert(p.pid);
                }
            }
            if found.len() == before {
                break;
            }
        }
        all.into_iter()
            .filter(|p| p.pid != self.pid && found.contains(&p.pid))
            .collect()
    }

    /// Kills one process by pidfd, after checking that the pid still names the
    /// incarnation that was found, and reaps it when it is our child (a
    /// subreaper adoption). A process that is not ours is left to its reaper.
    fn kill_one(victim: &observe::ProcInfo) {
        let Some(pid) = rustix::process::Pid::from_raw(victim.pid) else {
            return;
        };
        let Ok(fd) = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()) else {
            return;
        };
        // The pidfd pins the process now; check it is the one that was found.
        if !observe::process(victim.pid).is_some_and(|p| p.start_ticks == victim.start_ticks) {
            return;
        }
        let _ = rustix::process::pidfd_send_signal(&fd, rustix::process::Signal::KILL);
        // Waits for the exit by pidfd; only a child of ours can also be reaped.
        // Reaped (ours), or ECHILD (not ours): the kill was delivered. Only
        // EINTR is retried.
        while let Err(rustix::io::Errno::INTR) = rustix::process::waitid(
            rustix::process::WaitId::PidFd(fd.as_fd()),
            rustix::process::WaitIdOptions::EXITED,
        ) {}
    }

    /// Kill phase: the group and everything owned. The leader stays a zombie
    /// (its pid and group id reserved) and the readers keep running. Returns
    /// the owned processes that were still running.
    fn kill_phase(&mut self) -> Vec<observe::ProcInfo> {
        self.remember_descendants();
        // What is running before anything is killed: the group kill below
        // would otherwise hide it from the report.
        let mut killed: Vec<observe::ProcInfo> = self
            .owned_now()
            .into_iter()
            .filter(|p| p.state != 'Z')
            .collect();
        let group = rustix::process::Pid::from_raw(self.pid).expect("a child pid");
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        let _ = rustix::process::pidfd_send_signal(&self.pidfd, rustix::process::Signal::KILL);
        // The exit notification comes after the kernel has handed the leader's
        // children to the subreaper, so the discovery below sees them.
        if self.status.is_none() {
            self.status = self.exit.recv_timeout(Duration::from_secs(30)).ok();
        }
        let me = std::process::id() as i32;
        for _ in 0..20 {
            // Running ones are killed; exited ones that are our own children
            // (subreaper adoptions) are reaped. An exited process that is not
            // ours has nothing left to do.
            let pending: Vec<_> = self
                .owned_now()
                .into_iter()
                .filter(|p| p.state != 'Z' || p.ppid == me)
                .collect();
            if pending.is_empty() {
                break;
            }
            for p in pending {
                Self::kill_one(&p);
                if p.state != 'Z'
                    && !killed
                        .iter()
                        .any(|k| k.pid == p.pid && k.start_ticks == p.start_ticks)
                {
                    killed.push(p);
                }
            }
        }
        killed
    }

    /// Kills the child, its group and everything owned (the leader is reaped
    /// later, by `join` or the drop).
    pub fn kill_all(&mut self) {
        let _ = self.kill_phase();
    }

    /// Kill phase, then the leader's reap and the readers' join.
    fn reclaim(&mut self) -> (Vec<observe::ProcInfo>, usize) {
        let orphans = self.kill_phase();
        if let Some(reap) = self.reap.take() {
            let _ = reap.send(());
        }
        if let Some(waiter) = self.waiter.take() {
            let _ = self.reaped.recv_timeout(Duration::from_secs(30));
            let _ = waiter.join();
        }
        LEADERS.lock().unwrap().remove(&self.pid);
        self.stdin = None;
        let mut unjoined = 0;
        for (handle, done) in self.readers.drain(..) {
            match done.recv_timeout(self.reader_bound) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = handle.join();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => unjoined += 1,
            }
        }
        (orphans, unjoined)
    }

    /// The child must already have exited (or be terminated first). Reclaims
    /// everything and reports what was left behind.
    pub fn join(mut self) -> Report {
        let status = match self.status.or_else(|| self.note_exit()) {
            Some(status) => status,
            None => self
                .exit
                .recv_timeout(Duration::from_secs(30))
                .expect("a child that was asked to finish did not exit"),
        };
        self.status = Some(status);
        let (orphans, unjoined_readers) = self.reclaim();
        Report {
            status,
            stderr: self.stderr(),
            orphans,
            unjoined_readers,
        }
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        if self.waiter.is_some() {
            let _ = self.reclaim();
        }
    }
}
