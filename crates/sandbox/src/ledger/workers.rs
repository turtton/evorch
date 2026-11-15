//! The worker half of the ledger: monitoring tasks and blocking threads that
//! the core can wait for.

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use super::{Inner, SpawnLedger, locked};

/// Names one registered worker. Never reused within a ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkerId(u64);

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
            if let Some(record) = locked(&inner.workers).live.get_mut(&id) {
                record.thread = Some(handle);
            }
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
    pub async fn join_workers(&self) {
        loop {
            self.start_thread_joins();
            let mut activity = self.inner.workers_activity.subscribe();
            if locked(&self.inner.workers).live.is_empty() {
                return;
            }
            if activity.changed().await.is_err() {
                return;
            }
        }
    }

    /// Hand each unjoined thread to a joiner thread of the ledger, which
    /// removes the record once the thread is gone.
    fn start_thread_joins(&self) {
        let threads: Vec<_> = locked(&self.inner.workers)
            .live
            .iter_mut()
            .filter_map(|(id, record)| record.thread.take().map(|handle| (*id, handle)))
            .collect();
        for (id, handle) in threads {
            let inner = self.inner.clone();
            let spawned = std::thread::Builder::new()
                .name("worker-joiner".to_owned())
                .spawn(move || {
                    let _ = handle.join();
                    let mut table = locked(&inner.workers);
                    table.live.remove(&id);
                    table.epoch += 1;
                    inner.workers_activity.send_replace(table.activity());
                });
            if spawned.is_err() {
                // The handle went with the closure: the thread is detached
                // and its record can only be dropped, not proven joined.
                let mut table = locked(&self.inner.workers);
                table.live.remove(&id);
                table.epoch += 1;
                self.inner.workers_activity.send_replace(table.activity());
            }
        }
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
