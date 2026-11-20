//! A supervised child process (the leader of a family). Every wait is an event: a
//! line on its stdout, its exit notification, or the join of a reader thread.
//! The deadlines below exist only so that a hung child fails the test instead of
//! the runner; no deadline is ever the way two things are ordered.
//!
//! Who owns what
//! -------------
//! The leader is started by a reaper of its own ([`super::reaper`]), a process
//! that is the subreaper of this one family. Every process the family ever
//! has is therefore in the reaper's tree, and every orphan of the family is a
//! direct child of the reaper. This module never decides ownership by looking
//! at `/proc`: it asks the reaper, which kills and reaps only its own direct
//! children. The harness is not a subreaper and signals nothing but the leader,
//! through a pidfd, and the reaper's own control pipe.
//!
//! * The leader's pid is reserved (the reaper keeps it unreaped) until the
//!   cleanup, so the pidfd opened for it cannot name another process.
//! * The reaper dies with its control pipe: if the harness is gone, the family
//!   is killed and reaped by the reaper itself. If the reaper itself is killed,
//!   that is reported (`reaper_lost`); nothing is guessed.
//!
//! Cleanup does not depend on the test reaching its end: dropping the value
//! kills the family, reaps it and joins the readers.
use super::{observe, reaper};
use std::{
    io::{BufRead, BufReader, Write},
    os::{
        fd::OwnedFd,
        unix::process::{CommandExt, ExitStatusExt},
    },
    process::{Child, Command, ExitStatus, Stdio},
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
    /// Processes of the family that were still running when the cleanup began
    /// (the leader excluded). They were killed and reaped by the reaper; a
    /// non-empty list is a defect of the child, not of the fixture.
    pub orphans: Vec<observe::ProcInfo>,
    /// Reader threads that did not end within the bound because a process
    /// outside this family holds a pipe. They are detached.
    pub unjoined_readers: usize,
    /// The reaper did not answer or ended early: the family could not be
    /// proven reclaimed.
    pub reaper_lost: bool,
    /// What the reaper process wrote to its stderr.
    pub reaper_stderr: String,
}

/// Marker the environment helper puts on a command that must start with an
/// empty environment (std cannot be asked whether `env_clear` was called). It
/// is removed before the leader starts.
pub const CLEAR_ENV_MARKER: &str = "EVORCH_F4P_CLEAR_ENV";
/// Default bound for each reader thread's end after the family is dead.
const READER_JOIN_BOUND: Duration = Duration::from_secs(10);
const REPLY_WATCHDOG: Duration = Duration::from_secs(60);
const STDERR_LIMIT: usize = 256 * 1024;

/// Which step of a spawn fails, to test that a failure takes the family back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpawnFault {
    None,
    /// Opening the leader's pidfd fails after the reaper has started the leader.
    Pidfd,
    /// Duplicating that pidfd fails.
    PidfdClone,
}

thread_local! {
    /// (reaper pid, leader pid) of the last family a failed spawn on this thread
    /// took back.
    pub static LAST_ABORTED_SPAWN: std::cell::Cell<(i32, i32)> = const { std::cell::Cell::new((0, 0)) };
}

struct Events {
    leader: mpsc::Receiver<Result<serde_json::Value, String>>,
    exit: mpsc::Receiver<ExitStatus>,
    replies: mpsc::Receiver<(String, serde_json::Value)>,
}

pub struct ManagedProcess {
    pid: i32,
    reaper: Option<Child>,
    control: Option<std::process::ChildStdin>,
    pidfd: OwnedFd,
    spawned_at: Instant,
    lines: mpsc::Receiver<Line>,
    events: Events,
    status: Option<ExitStatus>,
    stdin: Option<std::fs::File>,
    stderr: Arc<Mutex<Vec<u8>>>,
    reaper_stderr: Arc<Mutex<Vec<u8>>>,
    readers: Vec<(JoinHandle<()>, mpsc::Receiver<()>)>,
    reader_bound: Duration,
    finished: bool,
}

fn spec_of(command: &Command) -> std::io::Result<reaper::Spec> {
    let utf8 = |s: &std::ffi::OsStr| {
        s.to_str()
            .map(str::to_owned)
            .ok_or_else(|| std::io::Error::other("a non-UTF-8 command is not supported"))
    };
    let mut cleared = false;
    let mut env: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let overrides: Vec<_> = command.get_envs().collect();
    for (name, value) in &overrides {
        if name.to_str() == Some(CLEAR_ENV_MARKER) {
            cleared = value.is_some();
        }
    }
    if !cleared {
        for (name, value) in std::env::vars_os() {
            if let (Some(name), Some(value)) = (name.to_str(), value.to_str()) {
                env.insert(name.to_owned(), value.to_owned());
            }
        }
    }
    for (name, value) in overrides {
        if name.to_str() == Some(CLEAR_ENV_MARKER) {
            continue;
        }
        match value {
            Some(value) => {
                env.insert(utf8(name)?, utf8(value)?);
            }
            None => {
                env.remove(&utf8(name)?);
            }
        }
    }
    Ok(reaper::Spec {
        program: utf8(command.get_program())?,
        args: command
            .get_args()
            .map(utf8)
            .collect::<std::io::Result<Vec<_>>>()?,
        env: env.into_iter().collect(),
        cwd: command
            .get_current_dir()
            .map(|d| utf8(d.as_os_str()))
            .transpose()?,
    })
}

/// Takes a family back after a failed spawn: the reaper's control pipe is closed
/// (the reaper kills and reaps the family), and the reaper process is reaped.
fn abort_spawn(mut reaper: Child, control: std::process::ChildStdin, leader: i32) {
    let reaper_pid = reaper.id() as i32;
    LAST_ABORTED_SPAWN.with(|last| last.set((reaper_pid, leader)));
    drop(control);
    let (tx, rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let _ = tx.send(reaper.wait());
    });
    if rx.recv_timeout(Duration::from_secs(30)).is_err() {
        // The reaper did not finish on its own: its group is killed as a last
        // resort. This is the only path that signals by group.
        if let Some(group) = rustix::process::Pid::from_raw(reaper_pid) {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
    }
    let _ = waiter.join();
}

impl ManagedProcess {
    pub fn spawn(command: Command) -> std::io::Result<Self> {
        Self::spawn_faulty(command, SpawnFault::None)
    }

    /// `spawn`, failing at `fault` when it is not `None` (tests only).
    pub fn spawn_faulty(command: Command, fault: SpawnFault) -> std::io::Result<Self> {
        let spec = spec_of(&command)?;
        let mut starter = Command::new(std::env::current_exe()?);
        starter
            .args(["--ignored", "--exact", reaper::ROLE_TEST, "--nocapture"])
            .env(reaper::ENV_SPEC, serde_json::to_string(&spec)?)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        // Only the target child is stopped from inheriting descriptors; this
        // process's own descriptor flags are left alone.
        sandbox::process::close_inherited_fds_on_exec(&mut starter);
        let mut reaper = starter.spawn()?;
        let reaper_pid = reaper.id() as i32;
        let control = reaper.stdin.take().expect("piped stdin");
        let events_out = reaper.stdout.take().expect("piped stdout");
        let reaper_err = reaper.stderr.take().expect("piped stderr");

        let (leader_tx, leader_rx) = mpsc::channel();
        let (exit_tx, exit_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        let mut readers = Vec::new();
        {
            let (done_tx, done_rx) = mpsc::channel();
            let handle = std::thread::spawn(move || {
                for line in BufReader::new(events_out).lines() {
                    let Ok(line) = line else { break };
                    let Some(body) = line.strip_prefix(reaper::EVENT_PREFIX) else {
                        continue;
                    };
                    let Ok(event) = serde_json::from_str::<serde_json::Value>(body) else {
                        continue;
                    };
                    let data = event["data"].clone();
                    match event["kind"].as_str().unwrap_or_default() {
                        "LEADER" => {
                            let _ = leader_tx.send(Ok(data));
                        }
                        "SPAWN_FAILED" => {
                            let _ = leader_tx.send(Err(data.to_string()));
                        }
                        "EXITED" => {
                            let raw = data.as_i64().unwrap_or(0) as i32;
                            let _ = exit_tx.send(ExitStatus::from_raw(raw));
                        }
                        kind => {
                            let _ = reply_tx.send((kind.to_owned(), data));
                        }
                    }
                }
                let _ = done_tx.send(());
            });
            readers.push((handle, done_rx));
        }
        let reaper_stderr = Arc::new(Mutex::new(Vec::new()));
        {
            let kept = reaper_stderr.clone();
            let (done_tx, done_rx) = mpsc::channel();
            let handle = std::thread::spawn(move || {
                let mut reader = BufReader::new(reaper_err);
                let mut chunk = Vec::new();
                loop {
                    chunk.clear();
                    match reader.read_until(b'\n', &mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let mut kept = kept.lock().unwrap();
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

        // The reaper starts the leader and says so.
        let leader = match leader_rx.recv_timeout(REPLY_WATCHDOG) {
            Ok(Ok(data)) => data,
            Ok(Err(why)) => {
                abort_spawn(reaper, control, 0);
                return Err(std::io::Error::other(format!(
                    "the reaper could not start the leader: {why}"
                )));
            }
            Err(_) => {
                abort_spawn(reaper, control, 0);
                return Err(std::io::Error::other(
                    "the reaper did not report a leader (is the f4p_family_reaper role declared in this test binary?)",
                ));
            }
        };
        let spawned_at = Instant::now();
        let pid = leader["pid"].as_i64().unwrap_or(0) as i32;
        // Every step that can fail from here on takes the family back at once,
        // before a reader of the leader's pipes starts.
        let opened =
            (|| -> std::io::Result<(OwnedFd, std::fs::File, std::fs::File, std::fs::File)> {
                let pidfd = match fault {
                    SpawnFault::Pidfd => Err(std::io::Error::other("injected: pidfd_open failed")),
                    _ => rustix::process::pidfd_open(
                        rustix::process::Pid::from_raw(pid)
                            .ok_or_else(|| std::io::Error::other("bad leader pid"))?,
                        rustix::process::PidfdFlags::empty(),
                    )
                    .map_err(std::io::Error::from),
                }?;
                let probe = match fault {
                    SpawnFault::PidfdClone => {
                        Err(std::io::Error::other("injected: pidfd clone failed"))
                    }
                    _ => pidfd.try_clone(),
                }?;
                drop(probe);
                let open = |key: &str, write: bool| {
                    let fd = leader[key].as_i64().unwrap_or(-1);
                    std::fs::OpenOptions::new()
                        .read(!write)
                        .write(write)
                        .open(format!("/proc/{reaper_pid}/fd/{fd}"))
                };
                Ok((
                    pidfd,
                    open("stdin_fd", true)?,
                    open("stdout_fd", false)?,
                    open("stderr_fd", false)?,
                ))
            })();
        let (pidfd, leader_in, leader_out, leader_err) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                abort_spawn(reaper, control, pid);
                return Err(error);
            }
        };
        let mut control = control;
        if writeln!(control, "ACK")
            .and_then(|()| control.flush())
            .is_err()
        {
            abort_spawn(reaper, control, pid);
            return Err(std::io::Error::other("the reaper's control pipe is closed"));
        }

        let (line_tx, lines) = mpsc::channel();
        {
            let (done_tx, done_rx) = mpsc::channel();
            let handle = std::thread::spawn(move || {
                for line in BufReader::new(leader_out).lines() {
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
        let stderr = Arc::new(Mutex::new(Vec::new()));
        {
            let kept = stderr.clone();
            let (done_tx, done_rx) = mpsc::channel();
            let handle = std::thread::spawn(move || {
                let mut reader = BufReader::new(leader_err);
                let mut chunk = Vec::new();
                loop {
                    chunk.clear();
                    match reader.read_until(b'\n', &mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let mut kept = kept.lock().unwrap();
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
        Ok(Self {
            pid,
            reaper: Some(reaper),
            control: Some(control),
            pidfd,
            spawned_at,
            lines,
            events: Events {
                leader: leader_rx,
                exit: exit_rx,
                replies: reply_rx,
            },
            status: None,
            stdin: Some(leader_in),
            stderr,
            reaper_stderr,
            readers,
            reader_bound: READER_JOIN_BOUND,
            finished: false,
        })
    }

    /// Changes the bound of the reader join (a failure bound, not a wait).
    pub fn set_reader_bound(&mut self, bound: Duration) {
        self.reader_bound = bound;
    }

    /// The leader's pid.
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// The pid of the family's reaper: the parent of the leader, and of every
    /// orphan of the family.
    pub fn reaper_pid(&self) -> i32 {
        self.reaper.as_ref().map_or(0, |r| r.id() as i32)
    }

    /// The instant the reaper reported that the leader was started.
    pub fn spawned_at(&self) -> Instant {
        self.spawned_at
    }

    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned()
    }

    fn note_exit(&mut self) -> Option<ExitStatus> {
        if self.status.is_none() {
            self.status = self.events.exit.try_recv().ok();
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
        self.status = self.events.exit.recv_timeout(watchdog).ok();
        self.status
    }

    /// The descendants of the leader right now, for observation only. Nothing
    /// is ever signalled because of this list.
    pub fn descendants(&self) -> Vec<observe::ProcInfo> {
        observe::descendants(self.pid)
    }

    /// Signals the leader through its pidfd. False when it is gone.
    pub fn signal(&self, signal: rustix::process::Signal) -> bool {
        rustix::process::pidfd_send_signal(&self.pidfd, signal).is_ok()
    }

    fn command(&mut self, text: &str) -> bool {
        match self.control.as_mut() {
            Some(control) => writeln!(control, "{text}")
                .and_then(|()| control.flush())
                .is_ok(),
            None => false,
        }
    }

    fn reply(&mut self, kind: &str, watchdog: Duration) -> Option<serde_json::Value> {
        let deadline = Instant::now() + watchdog;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.events.replies.recv_timeout(left) {
                Ok((got, data)) if got == kind => return Some(data),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    }

    /// Waits until every process of the family other than the leader has ended
    /// by itself (the reaper waits on each by pidfd and reaps it). Returns
    /// their exit statuses, or `None` past the watchdog or when the reaper is
    /// gone.
    pub fn join_family(&mut self, watchdog: Duration) -> Option<Vec<(i32, ExitStatus)>> {
        if !self.command("JOIN") {
            return None;
        }
        let data = self.reply("JOINED", watchdog)?;
        let joined: Vec<(i32, i32)> = serde_json::from_value(data).ok()?;
        Some(
            joined
                .into_iter()
                .map(|(pid, raw)| (pid, ExitStatus::from_raw(raw)))
                .collect(),
        )
    }

    /// SIGTERM to the leader, then its exit notification. Past the watchdog the
    /// family is killed (the caller then reports that the child ignored SIGTERM).
    pub fn terminate(&mut self, watchdog: Duration) -> Result<ExitStatus, WaitError> {
        self.signal(rustix::process::Signal::TERM);
        match self.wait_exit(watchdog) {
            Some(status) => Ok(status),
            None => {
                self.kill_all();
                Err(WaitError::TimedOut(watchdog))
            }
        }
    }

    /// Kills the family; the leader is reaped later, by `join` or the drop.
    pub fn kill_all(&mut self) {
        if self.command("KILL") {
            let _ = self.reply("KILLED", REPLY_WATCHDOG);
        }
    }

    /// Snapshot, kill, reap, and the join of the readers. Returns what was
    /// running when it began, whether the reaper was lost, and the readers that
    /// stayed open.
    fn reclaim(&mut self) -> (Vec<observe::ProcInfo>, bool, usize) {
        let mut lost = false;
        let mut orphans = Vec::new();
        if self.command("SNAPSHOT") {
            match self.reply("SNAPSHOT", REPLY_WATCHDOG) {
                Some(data) => orphans = serde_json::from_value(data).unwrap_or_default(),
                None => lost = true,
            }
        } else {
            lost = true;
        }
        if !lost && self.command("KILL") {
            lost |= self.reply("KILLED", REPLY_WATCHDOG).is_none();
        }
        if !lost && self.command("REAP") {
            lost |= self.reply("REAPED", REPLY_WATCHDOG).is_none();
        }
        self.control = None;
        self.stdin = None;
        if let Some(mut reaper) = self.reaper.take() {
            // After REAPED (or when the reaper was lost) its exit is imminent
            // or already happened; a bound keeps a failure a failure.
            let (tx, rx) = mpsc::channel();
            let waiter = std::thread::spawn(move || {
                let _ = tx.send(reaper.wait());
            });
            if rx.recv_timeout(Duration::from_secs(30)).is_err() {
                lost = true;
            }
            let _ = waiter.join();
        }
        let mut unjoined = 0;
        for (handle, done) in self.readers.drain(..) {
            match done.recv_timeout(self.reader_bound) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = handle.join();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => unjoined += 1,
            }
        }
        self.finished = true;
        (orphans, lost, unjoined)
    }

    /// The child must already have exited (or be terminated first). Reclaims
    /// the family and reports what was left behind.
    pub fn join(mut self) -> Report {
        let status = match self.status.or_else(|| self.note_exit()) {
            Some(status) => status,
            None => self
                .events
                .exit
                .recv_timeout(Duration::from_secs(30))
                .expect("a child that was asked to finish did not exit"),
        };
        self.status = Some(status);
        let (orphans, reaper_lost, unjoined_readers) = self.reclaim();
        Report {
            status,
            stderr: self.stderr(),
            orphans,
            unjoined_readers,
            reaper_lost,
            reaper_stderr: String::from_utf8_lossy(&self.reaper_stderr.lock().unwrap())
                .into_owned(),
        }
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.reclaim();
        }
    }
}
