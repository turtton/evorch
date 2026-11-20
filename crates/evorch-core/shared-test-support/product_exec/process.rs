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
    /// The leader's exit status; `None` when it was not reported (the reaper
    /// was lost, or the leader did not exit within the bound).
    pub status: Option<ExitStatus>,
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
    reaper_fd: OwnedFd,
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
    reply_bound: Duration,
    reaper_bound: Duration,
    /// Replies that arrived while another one was awaited.
    stash: Vec<(String, serde_json::Value)>,
    finished: bool,
}

/// What the reaper starts as the leader. Only these settings exist: a program,
/// arguments, an environment, a working directory. There is deliberately no
/// `Command` here, so that a setting (`pre_exec`, stdio, a user id) that the
/// reaper would silently drop cannot be written down at all. The reaper adds
/// the pipes and the process group itself.
#[derive(Debug, Clone, Default)]
pub struct LaunchSpec {
    program: String,
    args: Vec<String>,
    env: std::collections::BTreeMap<String, String>,
    clear_env: bool,
    cwd: Option<String>,
}

impl LaunchSpec {
    pub fn new(program: impl AsRef<std::ffi::OsStr>) -> Self {
        Self {
            program: program.as_ref().to_string_lossy().into_owned(),
            ..Self::default()
        }
    }
    pub fn arg(&mut self, arg: impl AsRef<std::ffi::OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_string_lossy().into_owned());
        self
    }
    pub fn args(
        &mut self,
        args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
    ) -> &mut Self {
        for arg in args {
            self.arg(arg);
        }
        self
    }
    pub fn env(
        &mut self,
        name: impl AsRef<std::ffi::OsStr>,
        value: impl AsRef<std::ffi::OsStr>,
    ) -> &mut Self {
        self.env.insert(
            name.as_ref().to_string_lossy().into_owned(),
            value.as_ref().to_string_lossy().into_owned(),
        );
        self
    }
    /// The leader starts with nothing but what `env` adds afterwards.
    pub fn env_clear(&mut self) -> &mut Self {
        self.clear_env = true;
        self.env.clear();
        self
    }
    pub fn current_dir(&mut self, dir: impl AsRef<std::path::Path>) -> &mut Self {
        self.cwd = Some(dir.as_ref().to_string_lossy().into_owned());
        self
    }

    fn into_reaper_spec(self) -> reaper::Spec {
        let mut env = std::collections::BTreeMap::new();
        if !self.clear_env {
            for (name, value) in std::env::vars_os() {
                if let (Some(name), Some(value)) = (name.to_str(), value.to_str()) {
                    env.insert(name.to_owned(), value.to_owned());
                }
            }
        }
        env.extend(self.env);
        reaper::Spec {
            program: self.program,
            args: self.args,
            env: env.into_iter().collect(),
            cwd: self.cwd,
        }
    }
}

/// Takes a family back after a failed spawn: the reaper's control pipe is closed
/// (the reaper kills and reaps the family) and the reaper process is reaped.
/// The last resort for a reaper that does not finish is a SIGKILL through the
/// pidfd that was opened while the reaper was still unreaped; no pid is used
/// after the wait may have reaped it, and the wait is not joined after the
/// bound.
fn abort_spawn(
    mut reaper: Child,
    reaper_fd: OwnedFd,
    control: std::process::ChildStdin,
    leader: i32,
) {
    let reaper_pid = reaper.id() as i32;
    LAST_ABORTED_SPAWN.with(|last| last.set((reaper_pid, leader)));
    drop(control);
    let (tx, rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let _ = tx.send(reaper.wait());
    });
    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(_) => {
            let _ = waiter.join();
        }
        Err(_) => {
            let _ = rustix::process::pidfd_send_signal(&reaper_fd, rustix::process::Signal::KILL);
        }
    }
}

impl ManagedProcess {
    pub fn spawn(spec: LaunchSpec) -> std::io::Result<Self> {
        Self::spawn_faulty(spec, SpawnFault::None)
    }

    /// `spawn`, failing at `fault` when it is not `None` (tests only).
    pub fn spawn_faulty(spec: LaunchSpec, fault: SpawnFault) -> std::io::Result<Self> {
        let spec = spec.into_reaper_spec();
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
        // The reaper is our own unreaped child, so this pidfd names it.
        let reaper_fd = match rustix::process::pidfd_open(
            rustix::process::Pid::from_raw(reaper_pid).expect("a child pid"),
            rustix::process::PidfdFlags::empty(),
        ) {
            Ok(fd) => fd,
            Err(error) => {
                let _ = reaper.kill();
                let _ = reaper.wait();
                return Err(error.into());
            }
        };
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
                abort_spawn(reaper, reaper_fd, control, 0);
                return Err(std::io::Error::other(format!(
                    "the reaper could not start the leader: {why}"
                )));
            }
            Err(_) => {
                abort_spawn(reaper, reaper_fd, control, 0);
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
                abort_spawn(reaper, reaper_fd, control, pid);
                return Err(error);
            }
        };
        let mut control = control;
        if writeln!(control, "ACK")
            .and_then(|()| control.flush())
            .is_err()
        {
            abort_spawn(reaper, reaper_fd, control, pid);
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
            reaper_fd,
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
            reply_bound: REPLY_WATCHDOG,
            reaper_bound: Duration::from_secs(30),
            stash: Vec::new(),
            finished: false,
        })
    }

    /// Changes the bound of the reader join (a failure bound, not a wait).
    pub fn set_reader_bound(&mut self, bound: Duration) {
        self.reader_bound = bound;
    }

    /// Changes the bounds of the cleanup: how long the reaper may take to answer
    /// a command, and how long its process may take to end. They are failure
    /// bounds: past one, the reaper is killed and reported lost.
    pub fn set_cleanup_bounds(&mut self, reply: Duration, reaper_exit: Duration) {
        self.reply_bound = reply;
        self.reaper_bound = reaper_exit;
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
        if let Some(at) = self.stash.iter().position(|(got, _)| got == kind) {
            return Some(self.stash.remove(at).1);
        }
        let deadline = Instant::now() + watchdog;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.events.replies.recv_timeout(left) {
                Ok((got, data)) if got == kind => return Some(data),
                Ok(other) => self.stash.push(other),
                Err(_) => return None,
            }
        }
    }

    /// Waits until every process of the family other than the leader has ended
    /// by itself (the reaper waits on each by pidfd and reaps it). Returns
    /// their exit statuses, or `None` past the watchdog or when the reaper is
    /// gone.
    pub fn join_family(&mut self, watchdog: Duration) -> Option<Vec<(i32, ExitStatus)>> {
        if !self.request_join() {
            return None;
        }
        self.await_join(watchdog)
    }

    /// Asks the reaper to start waiting for the family's other processes.
    pub fn request_join(&mut self) -> bool {
        self.command("JOIN") && self.reply("JOINING", self.reply_bound).is_some()
    }

    /// The answer to `request_join`.
    pub fn await_join(&mut self, watchdog: Duration) -> Option<Vec<(i32, ExitStatus)>> {
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
            let _ = self.reply("KILLED", self.reply_bound);
        }
    }

    /// Snapshot, kill, reap, and the join of the readers. Returns what was
    /// running when it began, whether the reaper was lost, and the readers that
    /// stayed open.
    fn reclaim(&mut self) -> (Vec<observe::ProcInfo>, bool, usize) {
        let mut lost = false;
        let mut orphans = Vec::new();
        if self.command("SNAPSHOT") {
            match self.reply("SNAPSHOT", self.reply_bound) {
                Some(data) => orphans = serde_json::from_value(data).unwrap_or_default(),
                None => lost = true,
            }
        } else {
            lost = true;
        }
        if !lost && self.command("KILL") {
            lost |= self.reply("KILLED", self.reply_bound).is_none();
        }
        if !lost && self.command("REAP") {
            lost |= self.reply("REAPED", self.reply_bound).is_none();
        }
        self.control = None;
        self.stdin = None;
        if let Some(mut reaper) = self.reaper.take() {
            // After REAPED (or when the reaper was lost) its exit is imminent
            // or already happened. The wait runs in a thread so that a reaper
            // that never finishes stays a failure: past the bound it is killed
            // through its pidfd, the thread is detached, and the report says
            // the reaper was lost.
            let (tx, rx) = mpsc::channel();
            let waiter = std::thread::spawn(move || {
                let _ = tx.send(reaper.wait());
            });
            match rx.recv_timeout(self.reaper_bound) {
                Ok(_) => {
                    let _ = waiter.join();
                }
                Err(_) => {
                    lost = true;
                    let _ = rustix::process::pidfd_send_signal(
                        &self.reaper_fd,
                        rustix::process::Signal::KILL,
                    );
                }
            }
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
        // A missing exit notification is a result, not a panic: the channel
        // ends when the reaper's output ends, which is reported as lost.
        let status = self
            .status
            .or_else(|| self.note_exit())
            .or_else(|| self.events.exit.recv_timeout(self.reply_bound).ok());
        self.status = status;
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

impl Report {
    /// The leader exited successfully and the reaper was not lost.
    pub fn success(&self) -> bool {
        !self.reaper_lost && self.status.is_some_and(|s| s.success())
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.reclaim();
        }
    }
}
