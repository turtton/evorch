//! Where the GUI and the core write the log that explains a lost connection.
//!
//! Every process writes its own file, `<name>-<pid>.log`, in one directory, so a
//! person who meets a disconnect reads `core-*.log` and `gui-*.log` side by
//! side and no two processes ever rotate or truncate each other's file. A file
//! is capped: when the next line would pass the cap it becomes
//! `<name>-<pid>.log.1` (replacing the previous one) and a new file starts. At
//! start-up the files of processes that have exited are pruned to the newest
//! few, so the directory stays bounded however often the programs restart.
//!
//! Writing never happens on the thread that logs: a line is queued (bounded,
//! never blocking) and one thread writes it. When the queue is full the line is
//! dropped and the next written line says how many were.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    time::Duration,
};

/// The size at which a log file is rotated.
pub const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
/// Lines that may wait for the writing thread.
pub const QUEUE_LINES: usize = 1024;
/// Files of exited processes kept per log name.
pub const KEEP_EXITED_FILES: usize = 8;
const FLUSH_WAIT: Duration = Duration::from_secs(2);

/// `$XDG_STATE_HOME/evorch/logs`, else `$HOME/.local/state/evorch/logs`.
pub fn log_dir() -> Option<PathBuf> {
    log_dir_from(
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

pub fn log_dir_from(xdg_state: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    let state = xdg_state
        .filter(|dir| !dir.as_os_str().is_empty())
        .or_else(|| {
            home.filter(|dir| !dir.as_os_str().is_empty())
                .map(|home| home.join(".local/state"))
        })?;
    Some(state.join("evorch").join("logs"))
}

/// An append-only log file with a size cap. Owner-only, as the lines name paths
/// and endpoints. One process owns a file, so rotating it races with nothing.
pub struct RotatingLog {
    path: PathBuf,
    rotated: PathBuf,
    file: File,
    size: u64,
    max: u64,
}

impl RotatingLog {
    /// Open `<dir>/<name>.log` for appending, creating the directory.
    pub fn open(dir: &Path, name: &str, max: u64) -> io::Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let path = dir.join(format!("{name}.log"));
        let rotated = dir.join(format!("{name}.log.1"));
        let (file, size) = open_append(&path)?;
        let mut log = Self {
            path,
            rotated,
            file,
            size,
            max,
        };
        if log.size >= log.max {
            log.rotate()?;
        }
        Ok(log)
    }

    /// This process's file for `name` (`<name>-<pid>.log`), after pruning the
    /// files that exited processes left.
    pub fn open_for_process(dir: &Path, name: &str, max: u64) -> io::Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        prune_exited(dir, name, std::process::id(), KEEP_EXITED_FILES);
        Self::open(dir, &format!("{name}-{}", std::process::id()), max)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn rotate(&mut self) -> io::Result<()> {
        fs::rename(&self.path, &self.rotated)?;
        let (file, size) = open_append(&self.path)?;
        self.file = file;
        self.size = size;
        Ok(())
    }
}

fn open_append(path: &Path) -> io::Result<(File, u64)> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    let size = file.metadata()?.len();
    Ok((file, size))
}

impl Write for RotatingLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.size > 0 && self.size + buf.len() as u64 > self.max {
            // Losing the rotation must not lose the line: keep appending.
            let _ = self.rotate();
        }
        let written = self.file.write(buf)?;
        self.size += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Delete the files of processes that no longer exist, keeping the newest
/// `keep`. The file of a running process is never touched, whatever its age.
fn prune_exited(dir: &Path, name: &str, own_pid: u32, keep: usize) {
    prune_exited_where(dir, name, keep, |pid| pid == own_pid || process_exists(pid));
}

fn prune_exited_where(dir: &Path, name: &str, keep: usize, running: impl Fn(u32) -> bool) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut exited = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(pid) = file_name.to_str().and_then(|text| pid_of(text, name)) else {
            continue;
        };
        if running(pid) {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        exited.push((modified, pid, entry.path()));
    }
    exited.sort_by_key(|(modified, ..)| std::cmp::Reverse(*modified));
    for (_, pid, path) in exited.into_iter().skip(keep) {
        // A pid can come back between the scan and here.
        if !running(pid) {
            let _ = fs::remove_file(path);
        }
    }
}

/// The pid in `<name>-<pid>.log` or `<name>-<pid>.log.1`.
fn pid_of(file_name: &str, name: &str) -> Option<u32> {
    let rest = file_name.strip_prefix(name)?.strip_prefix('-')?;
    let digits = rest
        .strip_suffix(".log")
        .or_else(|| rest.strip_suffix(".log.1"))?;
    digits.parse().ok()
}

fn process_exists(pid: u32) -> bool {
    let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return false;
    };
    match rustix::process::test_kill_process(pid) {
        Ok(()) => true,
        Err(error) => error != rustix::io::Errno::SRCH,
    }
}

/// How many lines have been handled (written, or dropped on a full queue), for
/// [`Logger::flush_within`].
#[derive(Default)]
struct Progress {
    written: Mutex<u64>,
    changed: Condvar,
}

impl Progress {
    fn handled_one(&self) {
        *self.written.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        self.changed.notify_all();
    }
}

/// Where lines are queued. Cheap to clone; never blocks the caller.
#[derive(Clone)]
pub struct LogSink {
    queue: SyncSender<Vec<u8>>,
    dropped: Arc<AtomicU64>,
    queued: Arc<AtomicU64>,
    progress: Arc<Progress>,
}

impl LogSink {
    /// A writer for one log line (what a `tracing` `MakeWriter` hands out). The
    /// line is queued when the writer is dropped.
    pub fn make_writer(&self) -> LineWriter {
        LineWriter {
            line: Vec::new(),
            sink: self.clone(),
        }
    }
}

pub struct LineWriter {
    line: Vec<u8>,
    sink: LogSink,
}

impl Write for LineWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.line.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for LineWriter {
    fn drop(&mut self) {
        if self.line.is_empty() {
            return;
        }
        // Counted before it is queued, so a flush that starts while this line
        // is on its way waits for it. A dropped line is accounted as handled.
        self.sink.queued.fetch_add(1, Ordering::AcqRel);
        if self
            .sink
            .queue
            .try_send(std::mem::take(&mut self.line))
            .is_err()
        {
            self.sink.dropped.fetch_add(1, Ordering::Relaxed);
            self.sink.progress.handled_one();
        }
    }
}

enum State<W> {
    /// Lines queue up but nothing writes them yet.
    Idle(W, Receiver<Vec<u8>>),
    Running,
    Stopped,
}

/// The queue and the thread that writes it. The thread is started by
/// [`Logger::start`], not by creating the logger: the core must create no
/// thread before its single-threaded entry work is done.
#[derive(Clone)]
pub struct Logger<W: Write + Send + 'static> {
    sink: LogSink,
    state: Arc<Mutex<State<W>>>,
}

impl<W: Write + Send + 'static> Logger<W> {
    pub fn new(writer: W, queue_lines: usize) -> Self {
        let (queue, lines) = mpsc::sync_channel(queue_lines);
        Self {
            sink: LogSink {
                queue,
                dropped: Arc::new(AtomicU64::new(0)),
                queued: Arc::new(AtomicU64::new(0)),
                progress: Arc::new(Progress::default()),
            },
            state: Arc::new(Mutex::new(State::Idle(writer, lines))),
        }
    }

    pub fn sink(&self) -> LogSink {
        self.sink.clone()
    }

    /// Start the writing thread. Does nothing when it already runs.
    pub fn start(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(*state, State::Idle(..)) {
            return;
        }
        let State::Idle(writer, lines) = std::mem::replace(&mut *state, State::Stopped) else {
            return;
        };
        let (dropped, progress) = (self.sink.dropped.clone(), self.sink.progress.clone());
        *state = match std::thread::Builder::new()
            .name("evorch-log".into())
            .spawn(move || write_lines(writer, &lines, &dropped, &progress))
        {
            Ok(_) => State::Running,
            Err(_) => State::Stopped,
        };
    }

    /// Wait until what was queued so far is written (the end of the process).
    /// Bounded: a stuck disk must not hold the exit.
    pub fn flush(&self) {
        self.flush_within(FLUSH_WAIT);
    }

    /// [`Logger::flush`] with its bound given; true when everything queued
    /// before the call was written. Nothing here waits for room in the queue:
    /// the wait is on a counter, so one deadline covers the whole call.
    /// [`Logger::flush`] with no bound, for callers that must see everything
    /// written (tests); a process that is ending uses [`Logger::flush`].
    pub fn flush_unbounded(&self) {
        self.start();
        let target = self.sink.queued.load(Ordering::Acquire);
        let progress = &self.sink.progress;
        let written = progress.written.lock().unwrap_or_else(|e| e.into_inner());
        drop(
            progress
                .changed
                .wait_while(written, |written| *written < target)
                .unwrap_or_else(|e| e.into_inner()),
        );
    }

    pub fn flush_within(&self, bound: Duration) -> bool {
        self.start();
        let target = self.sink.queued.load(Ordering::Acquire);
        let progress = &self.sink.progress;
        let written = progress.written.lock().unwrap_or_else(|e| e.into_inner());
        let (written, _) = progress
            .changed
            .wait_timeout_while(written, bound, |written| *written < target)
            .unwrap_or_else(|e| e.into_inner());
        *written >= target
    }
}

fn write_lines<W: Write>(
    mut writer: W,
    lines: &Receiver<Vec<u8>>,
    dropped: &AtomicU64,
    progress: &Progress,
) {
    for line in lines {
        let lost = dropped.swap(0, Ordering::Relaxed);
        if lost > 0 {
            let _ = writeln!(
                writer,
                "-- {lost} log lines were dropped: the log queue was full"
            );
        }
        // Handled whether or not the disk took it: a flush says the writer has
        // caught up, and must not wait for a line that can never be written.
        let _ = writer.write_all(&line);
        progress.handled_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_directory_follows_xdg_state_then_home() {
        let dir = |xdg: Option<&str>, home: Option<&str>| {
            log_dir_from(xdg.map(PathBuf::from), home.map(PathBuf::from))
        };
        assert_eq!(
            dir(Some("/state"), Some("/home/u")),
            Some("/state/evorch/logs".into())
        );
        assert_eq!(
            dir(Some(""), Some("/home/u")),
            Some("/home/u/.local/state/evorch/logs".into())
        );
        assert_eq!(dir(None, None), None);
    }

    #[test]
    fn a_full_file_is_rotated_once_and_keeps_the_newest_lines() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("logs");
        let mut log = RotatingLog::open(&dir, "core", 16).unwrap();
        log.write_all(b"0123456789\n").unwrap();
        log.write_all(b"abcdefghij\n").unwrap();
        log.write_all(b"ABCDEFGHIJ\n").unwrap();
        assert_eq!(fs::read(dir.join("core.log")).unwrap(), b"ABCDEFGHIJ\n");
        assert_eq!(fs::read(dir.join("core.log.1")).unwrap(), b"abcdefghij\n");
        drop(log);
        let log = RotatingLog::open(&dir, "core", 8).unwrap();
        assert_eq!(fs::read(log.path()).unwrap(), b"");
        assert_eq!(fs::read(dir.join("core.log.1")).unwrap(), b"ABCDEFGHIJ\n");
    }

    #[test]
    fn two_processes_write_two_files_so_neither_loses_lines_to_the_other_rotating() {
        let temp = tempfile::tempdir().unwrap();
        let mut first = RotatingLog::open(temp.path(), "core-11", 16).unwrap();
        let mut second = RotatingLog::open(temp.path(), "core-22", 16).unwrap();
        for round in 0..3 {
            first
                .write_all(format!("first {round}\n").as_bytes())
                .unwrap();
            second
                .write_all(format!("second {round}\n").as_bytes())
                .unwrap();
        }
        assert_eq!(
            fs::read(temp.path().join("core-11.log")).unwrap(),
            b"first 2\n"
        );
        assert_eq!(
            fs::read(temp.path().join("core-22.log")).unwrap(),
            b"second 2\n"
        );
        assert_eq!(
            fs::read(temp.path().join("core-11.log.1")).unwrap(),
            b"first 0\nfirst 1\n"
        );
    }

    #[test]
    fn the_files_of_exited_processes_are_pruned_to_the_newest_and_running_ones_stay() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let base = std::time::UNIX_EPOCH + Duration::from_secs(1_000_000);
        for (index, name) in [
            "core-1.log",
            "core-2.log",
            "core-3.log.1",
            "core-4.log",
            "gui-1.log",
            "core-notapid.log",
            "core-9.log",
        ]
        .into_iter()
        .enumerate()
        {
            let file = File::create(dir.join(name)).unwrap();
            file.set_modified(base + Duration::from_secs(index as u64))
                .unwrap();
        }
        // Pid 9 is running (and the oldest-looking would-be victim is irrelevant to it).
        prune_exited_where(dir, "core", 2, |pid| pid == 9);
        let mut left: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "core-3.log.1",
                "core-4.log",
                "core-9.log",
                "core-notapid.log",
                "gui-1.log"
            ],
            "the two newest exited files of `core`, the running one, and other names stay"
        );
    }

    #[test]
    fn a_queued_line_is_written_by_the_thread_after_start_and_flush() {
        let temp = tempfile::tempdir().unwrap();
        let log = RotatingLog::open(temp.path(), "t", 1 << 20).unwrap();
        let logger = Logger::new(log, 8);
        let sink = logger.sink();
        // Queued before the thread exists, as the core does at its entry.
        sink.make_writer().write_all(b"early\n").unwrap();
        logger.start();
        sink.make_writer().write_all(b"late\n").unwrap();
        wait_handled(&logger, 2);
        assert_eq!(
            fs::read(temp.path().join("t.log")).unwrap(),
            b"early\nlate\n"
        );
    }

    /// Wait, with no deadline, until the writer has handled `n` lines.
    fn wait_handled<W: Write + Send + 'static>(logger: &Logger<W>, n: u64) {
        let progress = &logger.sink.progress;
        let written = progress.written.lock().unwrap();
        drop(
            progress
                .changed
                .wait_while(written, |written| *written < n)
                .unwrap(),
        );
    }

    struct Gate {
        entered: mpsc::SyncSender<()>,
        release: Receiver<()>,
        out: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for Gate {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if buf == b"hold\n" {
                self.entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
            self.out.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_flush_returns_within_its_bound_while_the_writer_is_stuck_and_the_queue_is_full() {
        let (entered_tx, entered) = mpsc::sync_channel(1);
        let (release_tx, release) = mpsc::sync_channel(1);
        let out = Arc::new(Mutex::new(Vec::new()));
        let logger = Logger::new(
            Gate {
                entered: entered_tx,
                release,
                out: out.clone(),
            },
            1,
        );
        let sink = logger.sink();
        logger.start();
        sink.make_writer().write_all(b"hold\n").unwrap();
        entered.recv().unwrap();
        sink.make_writer().write_all(b"queued\n").unwrap();
        // The queue is full and the writer is stuck: waiting for room in the
        // queue here had no bound. The flush now reports "not written" instead.
        assert!(!logger.flush_within(Duration::ZERO));
        release_tx.send(()).unwrap();
        // Completion is the writer's own progress, awaited without a deadline.
        wait_handled(&logger, 2);
        assert!(logger.flush_within(Duration::ZERO));
        let text = String::from_utf8(out.lock().unwrap().clone()).unwrap();
        assert_eq!(text, "hold\nqueued\n");
    }

    #[test]
    fn a_full_queue_drops_lines_without_blocking_and_the_next_line_says_how_many() {
        let (entered_tx, entered) = mpsc::sync_channel(1);
        let (release_tx, release) = mpsc::sync_channel(1);
        let out = Arc::new(Mutex::new(Vec::new()));
        let logger = Logger::new(
            Gate {
                entered: entered_tx,
                release,
                out: out.clone(),
            },
            2,
        );
        let sink = logger.sink();
        logger.start();
        // The thread takes the first line and is held inside its write.
        sink.make_writer().write_all(b"hold\n").unwrap();
        entered.recv().unwrap();
        // Two lines fit in the queue; the next two are dropped, and none of
        // these calls waits for the held thread.
        for line in ["one\n", "two\n", "lost a\n", "lost b\n"] {
            sink.make_writer().write_all(line.as_bytes()).unwrap();
        }
        release_tx.send(()).unwrap();
        wait_handled(&logger, 5);
        let text = String::from_utf8(out.lock().unwrap().clone()).unwrap();
        assert!(text.contains("one\n") && text.contains("two\n"), "{text}");
        assert!(!text.contains("lost"), "{text}");
        assert!(text.contains("2 log lines were dropped"), "{text}");
    }
}
