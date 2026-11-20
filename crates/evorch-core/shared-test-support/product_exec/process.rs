//! A supervised child process. Every wait is an event: a line on its stdout, its
//! exit notification, or the join of a reader thread. The deadlines below
//! exist only so that a hung child fails the test instead of the runner; no
//! deadline is ever the way two things are ordered.
//!
//! Cleanup does not depend on the test reaching its end: dropping the value
//! kills the child and every descendant that is still alive, reaps it and joins
//! the reader threads.
use super::observe;
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::process::CommandExt,
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
    /// Descendants that were still alive when the child had exited. They were
    /// killed and reaped by the cleanup; a non-empty list is a defect of the
    /// child, not of the fixture.
    pub orphans: Vec<observe::ProcInfo>,
}

pub struct ManagedProcess {
    pid: i32,
    lines: mpsc::Receiver<Line>,
    exit: mpsc::Receiver<ExitStatus>,
    exited: Arc<Mutex<Option<ExitStatus>>>,
    status: Option<ExitStatus>,
    stdin: Option<ChildStdin>,
    stderr: Arc<Mutex<Vec<u8>>>,
    threads: Vec<JoinHandle<()>>,
    /// Descendants seen at the last observation, for cleanup after an exit.
    seen: Vec<observe::ProcInfo>,
}

const STDERR_LIMIT: usize = 256 * 1024;

impl ManagedProcess {
    /// The command's stdin/stdout/stderr are replaced by pipes. The child leads
    /// its own process group.
    pub fn spawn(mut command: Command) -> std::io::Result<Self> {
        static SUBREAPER: std::sync::Once = std::sync::Once::new();
        SUBREAPER.call_once(|| {
            rustix::process::set_child_subreaper(Some(rustix::process::getpid()))
                .expect("become a child subreaper");
        });
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child: Child = command.spawn()?;
        let pid = child.id() as i32;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr_pipe = child.stderr.take().expect("piped stderr");
        let stdin = child.stdin.take();
        let (line_tx, lines) = mpsc::channel();
        let (exit_tx, exit) = mpsc::channel();
        let exited = Arc::new(Mutex::new(None));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let mut threads = Vec::new();
        threads.push(std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(text) = line else { break };
                if line_tx
                    .send(Line {
                        at: Instant::now(),
                        text,
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
        {
            let stderr = stderr.clone();
            threads.push(std::thread::spawn(move || {
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
            }));
        }
        {
            let exited = exited.clone();
            threads.push(std::thread::spawn(move || {
                let status = child.wait().expect("wait for supervised child");
                // The slot is set before the notification, under the lock the
                // signalling side takes, so a signal never meets a reaped pid.
                *exited.lock().unwrap() = Some(status);
                let _ = exit_tx.send(status);
            }));
        }
        Ok(Self {
            pid,
            lines,
            exit,
            exited,
            status: None,
            stdin,
            stderr,
            threads,
            seen: Vec::new(),
        })
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

    /// Exit notification, if it arrives within the watchdog.
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
        let now = observe::descendants(self.pid);
        for p in now {
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

    pub fn signal(&self, signal: rustix::process::Signal) -> bool {
        let slot = self.exited.lock().unwrap();
        if slot.is_some() {
            return false;
        }
        rustix::process::Pid::from_raw(self.pid)
            .is_some_and(|pid| rustix::process::kill_process(pid, signal).is_ok())
    }

    /// SIGTERM, then the exit notification. Past the watchdog the group is
    /// killed (the test then reports that the child ignored SIGTERM).
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

    /// Kills the child, its group and every descendant seen or alive now, then
    /// joins them. The test process is a child subreaper, so a descendant that
    /// outlives its parent becomes this process's own child and can be waited
    /// for: the join is the kernel's exit notification, not a poll of `/proc`.
    pub fn kill_all(&mut self) {
        self.remember_descendants();
        let victims = self.seen.clone();
        {
            let slot = self.exited.lock().unwrap();
            if slot.is_none()
                && let Some(pid) = rustix::process::Pid::from_raw(self.pid)
            {
                let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
        }
        // The leader's exit is reported after the kernel has handed its
        // children to the subreaper, so the waits below can find them.
        if self.status.is_none() {
            self.status = self.exit.recv_timeout(Duration::from_secs(30)).ok();
        }
        for victim in victims {
            // Only the incarnation that was seen, never a reused pid.
            if !observe::process(victim.pid).is_some_and(|p| p.start_ticks == victim.start_ticks) {
                continue;
            }
            let Some(pid) = rustix::process::Pid::from_raw(victim.pid) else {
                continue;
            };
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            // Retried on EINTR only. Any other result: reaped, or not our child
            // (then init reaps it).
            while let Err(rustix::io::Errno::INTR) = rustix::process::waitid(
                rustix::process::WaitId::Pid(pid),
                rustix::process::WaitIdOptions::EXITED,
            ) {}
        }
    }

    /// The child must already have exited (or be terminated first). Joins the
    /// reader threads and reports what was left behind.
    pub fn join(mut self) -> Report {
        let status = match self.status.or_else(|| self.note_exit()) {
            Some(status) => status,
            None => self
                .exit
                .recv_timeout(Duration::from_secs(30))
                .expect("a child that was asked to finish did not exit"),
        };
        self.status = Some(status);
        let orphans: Vec<_> = self
            .seen
            .iter()
            .filter(|p| !observe::is_gone(p))
            .cloned()
            .collect();
        // Kills and reaps; the threads end when the pipes close.
        self.kill_all();
        let stderr = self.finish_threads();
        Report {
            status,
            stderr,
            orphans,
        }
    }

    fn finish_threads(&mut self) -> String {
        self.stdin = None;
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        self.stderr()
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        if self.threads.is_empty() {
            return;
        }
        self.kill_all();
        self.finish_threads();
    }
}
