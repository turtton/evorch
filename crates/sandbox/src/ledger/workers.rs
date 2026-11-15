//! The worker half of the ledger: monitoring tasks and blocking threads that
//! the core can wait for.

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use super::{Inner, SpawnLedger, locked};

/// Names one registered worker. Never reused within a ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkerId(u64);

/// A worker thread could not be joined, because no thread could be started
/// to join it. The worker stays registered, so nothing is taken for ended; call
/// [`SpawnLedger::join_workers`] again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("no joiner could be started for {unjoined} worker thread(s); they stay registered")]
pub struct JoinFailed {
    pub unjoined: usize,
}

/// Hooks for tests that need to fix an order or inject a failure.
#[cfg(test)]
#[derive(Default)]
pub(super) struct Seams {
    /// How many of the next joiner threads fail to start.
    pub fail_joiner_starts: std::sync::atomic::AtomicUsize,
    /// Held by `spawn_thread` before it publishes the thread's handle.
    pub hold_publish: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    /// Run once by `join_workers`, after it has started its joins and before it
    /// looks at what is left.
    pub join_window: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Run once by the next attempt to start a joiner thread, before it starts.
    pub joiner_start: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

#[cfg(test)]
impl std::fmt::Debug for Seams {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Seams").finish_non_exhaustive()
    }
}

/// A running worker, as [`SpawnLedger::workers`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerInfo {
    pub id: WorkerId,
    pub name: &'static str,
    pub is_thread: bool,
}

/// How many workers the ledger has seen end, and how.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkerStats {
    pub spawned: u64,
    pub finished: u64,
    /// Ended by panicking (a task that was aborted is not a panic).
    pub panicked: u64,
}

/// The ledger's workers as observers see them. `epoch` changes on every
/// registration and every departure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkerActivity {
    pub live: usize,
    pub epoch: u64,
}

#[derive(Debug)]
struct Record {
    name: &'static str,
    abort: Option<tokio::task::AbortHandle>,
    thread: Option<std::thread::JoinHandle<()>>,
    is_thread: bool,
}

#[derive(Debug, Default)]
pub(super) struct Table {
    next: u64,
    live: BTreeMap<WorkerId, Record>,
    stats: WorkerStats,
    epoch: u64,
}

impl Table {
    fn activity(&self) -> WorkerActivity {
        WorkerActivity {
            live: self.live.len(),
            epoch: self.epoch,
        }
    }
}

/// Counts a worker's end when its body ends, however it ends: returning,
/// panicking, or being dropped by an abort. A task's record leaves with it. A
/// thread's record stays until the thread is joined, so that the join can
/// prove the thread itself is gone.
struct Finish {
    inner: Arc<Inner>,
    id: WorkerId,
    is_thread: bool,
}

impl Drop for Finish {
    fn drop(&mut self) {
        let mut table = locked(&self.inner.workers);
        table.stats.finished += 1;
        if std::thread::panicking() {
            table.stats.panicked += 1;
        }
        if !self.is_thread {
            table.live.remove(&self.id);
        }
        table.epoch += 1;
        self.inner.workers_activity.send_replace(table.activity());
    }
}

/// A task body that counts its end only after everything it owns is dropped:
/// fields drop in declaration order, so the future (and what it captured) goes
/// first and the record is removed last.
struct Guarded {
    future: Pin<Box<dyn Future<Output = ()> + Send>>,
    _finish: Finish,
}

impl Future for Guarded {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.future.as_mut().poll(cx)
    }
}

fn enroll(inner: &Arc<Inner>, name: &'static str, is_thread: bool) -> Finish {
    let mut table = locked(&inner.workers);
    let id = WorkerId(table.next);
    table.next += 1;
    table.live.insert(
        id,
        Record {
            name,
            abort: None,
            thread: None,
            is_thread,
        },
    );
    table.stats.spawned += 1;
    table.epoch += 1;
    inner.workers_activity.send_replace(table.activity());
    drop(table);
    Finish {
        inner: inner.clone(),
        id,
        is_thread,
    }
}

pub(super) fn spawn_task<F>(inner: &Arc<Inner>, name: &'static str, future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let finish = enroll(inner, name, false);
    let id = finish.id;
    let handle = tokio::spawn(Guarded {
        future: Box::pin(future),
        _finish: finish,
    });
    // The task may already have finished and removed its record.
    if let Some(record) = locked(&inner.workers).live.get_mut(&id) {
        record.abort = Some(handle.abort_handle());
    }
}

pub(super) fn spawn_thread<F>(inner: &Arc<Inner>, name: &'static str, work: F) -> io::Result<()>
where
    F: FnOnce() + Send + 'static,
{
    let finish = enroll(inner, name, true);
    let id = finish.id;
    let spawned = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let _finish = finish;
            work();
        });
    match spawned {
        Ok(handle) => {
            #[cfg(test)]
            if let Some(hold) = locked(&inner.seams.hold_publish).take() {
                let _ = hold.recv();
            }
            // Publishing the handle is a change a waiting join must see: the
            // thread may have ended already, and nothing else will wake it.
            let mut table = locked(&inner.workers);
            if let Some(record) = table.live.get_mut(&id) {
                record.thread = Some(handle);
            }
            table.epoch += 1;
            inner.workers_activity.send_replace(table.activity());
            Ok(())
        }
        Err(error) => {
            // The closure was dropped unrun, which counted as a finish.
            let mut table = locked(&inner.workers);
            table.live.remove(&id);
            table.epoch += 1;
            inner.workers_activity.send_replace(table.activity());
            Err(error)
        }
    }
}

impl SpawnLedger {
    /// The workers that have not ended (and, for threads, been joined).
    pub fn workers(&self) -> Vec<WorkerInfo> {
        locked(&self.inner.workers)
            .live
            .iter()
            .map(|(id, record)| WorkerInfo {
                id: *id,
                name: record.name,
                is_thread: record.is_thread,
            })
            .collect()
    }

    pub fn worker_stats(&self) -> WorkerStats {
        locked(&self.inner.workers).stats
    }

    /// A watch on the live worker count.
    pub fn watch_workers(&self) -> tokio::sync::watch::Receiver<WorkerActivity> {
        self.inner.workers_activity.subscribe()
    }

    /// Abort every registered task; returns how many were asked to stop.
    ///
    /// A task ends at its next await point. Blocking worker threads cannot be
    /// aborted: stop them through whatever they wait on, then join them.
    pub fn abort_workers(&self) -> usize {
        let table = locked(&self.inner.workers);
        table
            .live
            .values()
            .filter_map(|record| record.abort.as_ref())
            .map(tokio::task::AbortHandle::abort)
            .count()
    }

    /// Wait until every registered worker has ended: tasks have finished or
    /// been dropped by an abort, and threads have been joined. With the fence
    /// open, workers registered meanwhile are waited for too.
    ///
    /// Does not stop anything and has no deadline; a worker that never ends
    /// keeps this pending, so bound it from outside.
    ///
    /// Cancel-safe: the joining of threads and the removal of their records is
    /// done by the ledger, not by this future, so a join that is dropped can
    /// simply be called again.
    ///
    /// # Errors
    /// [`JoinFailed`] when a joiner thread could not be started. The worker
    /// stays registered (it is not reported as ended); calling again retries.
    pub async fn join_workers(&self) -> Result<(), JoinFailed> {
        loop {
            // Subscribe before looking at anything: every change after this
            // point, such as a thread's handle being published or a worker
            // ending, is then seen by `changed`.
            let mut activity = self.inner.workers_activity.subscribe();
            let unjoined = self.start_thread_joins();
            #[cfg(test)]
            if let Some(window) = locked(&self.inner.seams.join_window).take() {
                window();
            }
            if unjoined > 0 {
                return Err(JoinFailed { unjoined });
            }
            if locked(&self.inner.workers).live.is_empty() {
                return Ok(());
            }
            if activity.changed().await.is_err() {
                return Ok(());
            }
        }
    }

    /// Hand each unjoined thread to a joiner thread of the ledger, which
    /// removes the record once the thread is gone. Returns how many could not
    /// be handed over: their handle goes back to the record, which stays.
    fn start_thread_joins(&self) -> usize {
        let threads: Vec<_> = locked(&self.inner.workers)
            .live
            .iter_mut()
            .filter_map(|(id, record)| record.thread.take().map(|handle| (*id, handle)))
            .collect();
        let mut unjoined = 0;
        for (id, handle) in threads {
            // The handle waits in a slot until the joiner runs, so a failed
            // start can take it back instead of losing the thread.
            let slot = Arc::new(Mutex::new(Some(handle)));
            let joiner_slot = Arc::clone(&slot);
            let inner = self.inner.clone();
            let started = self.start_joiner(move || {
                if let Some(handle) = locked(&joiner_slot).take() {
                    let _ = handle.join();
                }
                let mut table = locked(&inner.workers);
                table.live.remove(&id);
                table.epoch += 1;
                inner.workers_activity.send_replace(table.activity());
            });
            if started.is_err() {
                unjoined += 1;
                let handle = locked(&slot).take();
                // Giving the handle back is a change a concurrent join must
                // see: it found the record without a handle (this call had
                // taken it), is waiting on the activity, and nothing else
                // would wake it to start the join itself.
                let mut table = locked(&self.inner.workers);
                if let Some(record) = table.live.get_mut(&id) {
                    record.thread = handle;
                }
                table.epoch += 1;
                self.inner.workers_activity.send_replace(table.activity());
            }
        }
        unjoined
    }

    fn start_joiner(&self, join: impl FnOnce() + Send + 'static) -> io::Result<()> {
        #[cfg(test)]
        {
            use std::sync::atomic::Ordering;
            if let Some(hook) = locked(&self.inner.seams.joiner_start).take() {
                hook();
            }
            let failing = &self.inner.seams.fail_joiner_starts;
            if failing
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
            {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
        }
        std::thread::Builder::new()
            .name("worker-joiner".to_owned())
            .spawn(join)
            .map(drop)
    }

    /// Run cleanup of something the core already owns on a thread the ledger
    /// tracks, so [`join_workers`](Self::join_workers) waits for it.
    ///
    /// Unlike [`Admission::spawn_worker_thread`] this is not refused by the
    /// closed fence: it starts no new work, it finishes the release of what
    /// was already started (a supervisor's fallback cleanup runs exactly when
    /// the restart is shutting things down).
    ///
    /// # Errors
    /// The error of starting the thread; `work` is dropped unrun.
    pub fn spawn_cleanup_thread<F>(&self, name: &'static str, work: F) -> io::Result<()>
    where
        F: FnOnce() + Send + 'static,
    {
        spawn_thread(&self.inner, name, work)
    }
}
