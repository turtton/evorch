//! Following state that other GUI processes write to the shared database and
//! `sidebar.json`.
//!
//! A [`FeedPoller`] watches the event log through a read-only SQLite connection.
//! It asks SQLite only for `PRAGMA data_version`, which changes when another
//! connection commits, and reads events only after it changed. A background
//! worker ([`FeedWorker`]) runs it on an adaptive cadence and hands batches to
//! the GUI thread; tests call [`FeedPoller::poll_once`] directly.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use event_bus::{Event, EventKind, OwnershipAction};
use storage::{Database, StorageError};

/// Events read from the log per query.
pub const PAGE_SIZE: usize = 256;
/// Rows scanned in one poll; a longer backlog continues at the next poll.
pub const POLL_ROW_LIMIT: usize = 2048;
/// Poll interval while another process works or something changed recently.
pub const ACTIVE_INTERVAL: Duration = Duration::from_millis(200);
/// Poll interval while nothing is going on.
pub const IDLE_INTERVAL: Duration = Duration::from_secs(1);
/// How long after the last observed change the active interval is kept.
pub const RECENT_CHANGE_WINDOW: Duration = Duration::from_secs(3);

/// How long to wait before the next poll.
#[must_use]
pub fn next_interval(foreign_active: bool, since_change: Duration, backlog: bool) -> Duration {
    if backlog {
        Duration::ZERO
    } else if foreign_active || since_change < RECENT_CHANGE_WINDOW {
        ACTIVE_INTERVAL
    } else {
        IDLE_INTERVAL
    }
}

/// Identity of a file's content as far as `stat` can tell. Every save replaces
/// the file, so the inode and modification time change with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
    inode: Option<u64>,
}

impl FileStamp {
    /// The stamp of `path`, or `None` when it cannot be read.
    #[must_use]
    pub fn of(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            Some(metadata.ino())
        };
        #[cfg(not(unix))]
        let inode = None;
        Some(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            inode,
        })
    }
}

/// Runs of a thread whose owner released it, with how each last ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasedThread {
    pub thread_id: String,
    pub runs: Vec<ReleasedRun>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasedRun {
    pub run_id: String,
    /// The phase at the run's last saved snapshot (`Done`, `Error`, `Stopped`,
    /// `Checkpoint`), if one was saved.
    pub terminal_phase: Option<String>,
}

/// What one poll found.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct FeedBatch {
    /// Events other processes persisted, in log order.
    pub events: Vec<Event>,
    /// Threads another process released or handed off in `events`.
    pub released: Vec<ReleasedThread>,
    /// The `sidebar.json` stamp when it differs from the previous poll's.
    pub sidebar_stamp: Option<FileStamp>,
    /// More rows were left for the next poll.
    pub backlog: bool,
}

impl FeedBatch {
    fn is_empty(&self) -> bool {
        self.events.is_empty() && self.sidebar_stamp.is_none()
    }
}

/// Where a follower starts and what it must not fold twice.
#[derive(Debug, Clone)]
pub struct FeedConfig {
    pub db_path: PathBuf,
    /// The stream id this process persists under; its rows are skipped.
    pub own_stream: String,
    /// The last event ID already reflected in the GUI.
    pub cursor: i64,
    pub sidebar_path: Option<PathBuf>,
}

/// Synchronous core of the follower.
pub struct FeedPoller {
    db: Database,
    own_stream: String,
    cursor: i64,
    data_version: Option<i64>,
    sidebar_path: Option<PathBuf>,
    reported_stamp: Option<FileStamp>,
    event_queries: usize,
}

impl FeedPoller {
    /// Opens a read-only connection to the shared database.
    ///
    /// # Errors
    /// Fails when the database cannot be opened read-only.
    pub fn open(config: FeedConfig) -> Result<Self, StorageError> {
        Ok(Self {
            db: Database::open_read_only(&config.db_path)?,
            own_stream: config.own_stream,
            cursor: config.cursor,
            data_version: None,
            sidebar_path: config.sidebar_path,
            reported_stamp: None,
            event_queries: 0,
        })
    }

    /// How many event queries have run; a poll that finds `data_version`
    /// unchanged runs none.
    #[must_use]
    pub const fn event_queries(&self) -> usize {
        self.event_queries
    }

    /// The last event ID scanned.
    #[must_use]
    pub const fn cursor(&self) -> i64 {
        self.cursor
    }

    /// One poll: events are read only when `data_version` moved. The sidebar
    /// stamp is read afterwards, so a thread an owner created (saving the
    /// sidebar first and persisting events second) is in the file by the time
    /// its events are seen.
    ///
    /// # Errors
    /// Fails when the first query of the poll fails.
    pub fn poll_once(&mut self) -> Result<FeedBatch, StorageError> {
        let mut batch = FeedBatch::default();
        let version = self.db.data_version()?;
        if self.data_version != Some(version) {
            let mut scanned = 0;
            let mut complete = false;
            while scanned < POLL_ROW_LIMIT {
                self.event_queries += 1;
                let page =
                    match self
                        .db
                        .events_after_excluding(self.cursor, PAGE_SIZE, &self.own_stream)
                    {
                        Ok(page) => page,
                        Err(error) if scanned > 0 => {
                            tracing::warn!(%error, "shared event feed stopped mid-poll");
                            break;
                        }
                        Err(error) => return Err(error),
                    };
                self.cursor = page.last_id;
                scanned += page.scanned;
                batch
                    .events
                    .extend(page.events.into_iter().map(|stored| stored.event));
                if page.scanned < PAGE_SIZE {
                    complete = true;
                    break;
                }
            }
            if complete {
                self.data_version = Some(version);
            } else {
                batch.backlog = true;
            }
            batch.released = self.released_threads(&batch.events);
        }
        if let Some(path) = &self.sidebar_path {
            let stamp = FileStamp::of(path);
            if stamp.is_some() && stamp != self.reported_stamp {
                self.reported_stamp.clone_from(&stamp);
                batch.sidebar_stamp = stamp;
            }
        }
        Ok(batch)
    }

    fn released_threads(&self, events: &[Event]) -> Vec<ReleasedThread> {
        let mut threads: Vec<&str> = Vec::new();
        for event in events {
            if let EventKind::Ownership(ownership) = &event.kind
                && matches!(
                    ownership.action,
                    OwnershipAction::Released | OwnershipAction::Handoff
                )
                && !threads.contains(&ownership.thread_id.as_str())
            {
                threads.push(&ownership.thread_id);
            }
        }
        threads
            .into_iter()
            .filter_map(|thread_id| {
                let run_ids: Vec<String> = match self.db.runs_of_thread(thread_id) {
                    Ok(rows) => rows.into_iter().map(|row| row.run_id).collect(),
                    Err(error) => {
                        tracing::warn!(%error, thread_id, "released thread runs unreadable");
                        return None;
                    }
                };
                let summaries = self.db.terminal_summaries(&run_ids).unwrap_or_default();
                let runs = run_ids
                    .into_iter()
                    .map(|run_id| ReleasedRun {
                        terminal_phase: summaries
                            .iter()
                            .find(|summary| summary.run_id == run_id)
                            .map(|summary| summary.terminal_phase.clone()),
                        run_id,
                    })
                    .collect();
                Some(ReleasedThread {
                    thread_id: thread_id.to_owned(),
                    runs,
                })
            })
            .collect()
    }
}

enum Control {
    Stop,
}

/// A [`FeedPoller`] running on its own thread. Dropping it stops the thread.
pub struct FeedWorker {
    batches: Option<mpsc::Receiver<FeedBatch>>,
    control: mpsc::Sender<Control>,
    foreign_active: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FeedWorker {
    /// Starts polling. `repaint` is called whenever a batch is queued.
    ///
    /// # Errors
    /// Fails when the polling thread cannot be spawned.
    pub fn spawn(
        mut poller: FeedPoller,
        repaint: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> std::io::Result<Self> {
        Self::spawn_source(move || poller.poll_once(), repaint)
    }

    /// [`Self::spawn`] over any source of batches.
    ///
    /// # Errors
    /// Fails when the polling thread cannot be spawned.
    pub fn spawn_source(
        mut poll: impl FnMut() -> Result<FeedBatch, StorageError> + Send + 'static,
        repaint: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> std::io::Result<Self> {
        let (batch_tx, batches) = mpsc::sync_channel(16);
        let (control, control_rx) = mpsc::channel();
        let foreign_active = Arc::new(AtomicBool::new(false));
        let active = Arc::clone(&foreign_active);
        let thread = std::thread::Builder::new()
            .name("evorch-shared-feed".into())
            .spawn(move || {
                let mut last_change = Instant::now();
                let mut backlog = false;
                loop {
                    let wait = next_interval(
                        active.load(Ordering::Relaxed),
                        last_change.elapsed(),
                        backlog,
                    );
                    match control_rx.recv_timeout(wait) {
                        Ok(Control::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    match poll() {
                        Ok(batch) => {
                            backlog = batch.backlog;
                            if batch.is_empty() {
                                continue;
                            }
                            last_change = Instant::now();
                            if batch_tx.send(batch).is_err() {
                                break;
                            }
                            if let Some(repaint) = &repaint {
                                repaint();
                            }
                        }
                        Err(error) => {
                            backlog = false;
                            tracing::warn!(%error, "shared event feed poll failed");
                        }
                    }
                }
            })?;
        Ok(Self {
            batches: Some(batches),
            control,
            foreign_active,
            thread: Some(thread),
        })
    }

    /// Takes the next queued batch without waiting.
    pub fn try_next(&self) -> Option<FeedBatch> {
        self.batches.as_ref()?.try_recv().ok()
    }

    /// Tells the worker whether a run owned elsewhere is in progress, which
    /// keeps it on the active cadence.
    pub fn set_foreign_active(&self, active: bool) {
        self.foreign_active.store(active, Ordering::Relaxed);
    }
}

impl Drop for FeedWorker {
    fn drop(&mut self) {
        // A worker blocked on a full queue never reads its control channel;
        // without a receiver its send fails and it leaves the loop.
        drop(self.batches.take());
        let _ = self.control.send(Control::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cadence_is_active_for_foreign_work_or_recent_change_and_idle_otherwise() {
        let long = Duration::from_secs(60);
        assert_eq!(next_interval(false, long, false), IDLE_INTERVAL);
        assert_eq!(next_interval(true, long, false), ACTIVE_INTERVAL);
        assert_eq!(
            next_interval(false, RECENT_CHANGE_WINDOW / 2, false),
            ACTIVE_INTERVAL
        );
        assert_eq!(next_interval(false, long, true), Duration::ZERO);
    }
}
