//! The core's ledger of the child processes and worker tasks it owns, and the
//! final spawn fence in front of it.
//!
//! A planned restart ends in `execve`, which runs no `Drop`. Cleanup that
//! relies on `kill_on_drop` or on a destructor therefore never happens, and a
//! child or a task that outlives the old image is orphaned. So whatever the
//! core starts is registered here, while it is started, and the core stops and
//! reaps it by name:
//!
//! * **Children.** [`Admission::spawn_group`] and [`Admission::register_group`]
//!   enter a child in the ledger. [`SpawnLedger::stop_child`],
//!   [`terminate_child`](SpawnLedger::terminate_child) and
//!   [`stop_all_children`](SpawnLedger::stop_all_children) signal the process
//!   group of a registered child only: there is no way to name a process by its
//!   pid, so a process the ledger does not own is never signalled.
//!   [`reap_child`](SpawnLedger::reap_child) and
//!   [`reap_all_children`](SpawnLedger::reap_all_children) resolve once the
//!   owner has reaped the leader. The owner keeps its own reaping logic (a
//!   child's pipes, grace periods and admission permits stay where they were);
//!   the ledger records that it happened.
//! * **Workers.** [`Admission::spawn_worker`] and
//!   [`Admission::spawn_worker_thread`] start a monitoring task or a blocking
//!   worker that [`SpawnLedger::join_workers`] waits for.
//! * **Fence.** [`SpawnLedger::close_fence`] is one-way and exclusive with
//!   spawning. A spawn runs inside an [`Admission`] that holds the fence's read
//!   side from the check until the child or worker is registered, and closing
//!   takes the write side. So everything started before the fence is in the
//!   ledger when it returns, and nothing starts after it.
//!
//! The recommended order is [`close_fence`](SpawnLedger::close_fence), then
//! [`stop_all_children`](SpawnLedger::stop_all_children) and
//! [`reap_all_children`](SpawnLedger::reap_all_children), then
//! [`join_workers`](SpawnLedger::join_workers): with the fence closed nothing
//! can replace what has been stopped.
//!
//! A site that has no ledger uses [`Admission::unmanaged`] (or
//! [`SpawnLedger::admit_optional`] with `None`) and behaves as it did before.

mod children;
mod workers;

use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard};

pub use children::{
    ChildActivity, ChildEntry, ChildId, ChildInfo, ChildKind, ChildStats, UnconfirmedReap,
};
pub use workers::{JoinFailed, WorkerActivity, WorkerId, WorkerInfo, WorkerStats};

use crate::process::{GroupChild, ProcessGroup};

/// Why a child or a worker could not be started through the ledger.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    /// The final spawn fence is closed; nothing new may be started.
    #[error("the spawn fence is closed")]
    Fenced,
    #[error("{0}")]
    Io(#[from] io::Error),
}

impl From<SpawnError> for io::Error {
    fn from(error: SpawnError) -> Self {
        match error {
            SpawnError::Fenced => Self::other(error),
            SpawnError::Io(error) => error,
        }
    }
}

/// Where the final spawn fence stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceState {
    Open,
    /// [`SpawnLedger::close_fence`] has begun and waits for the admissions in
    /// flight; new ones are not guaranteed to succeed.
    Closing,
    Closed,
}

/// A [`ChildId`] the ledger does not hold: never registered, or already gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("child {0:?} is not in the ledger")]
pub struct UnknownChild(pub ChildId);

/// The shared state behind every clone of a [`SpawnLedger`].
#[derive(Debug)]
struct Inner {
    /// `true` once closed. Spawns hold the read side, closing takes the write
    /// side, which waits for the spawns in flight.
    fence: RwLock<bool>,
    /// Lock-free view of the fence for observers; written under the write side.
    fenced: AtomicBool,
    fence_state: tokio::sync::watch::Sender<FenceState>,
    children: Mutex<children::Table>,
    children_activity: tokio::sync::watch::Sender<ChildActivity>,
    workers: Mutex<workers::Table>,
    workers_activity: tokio::sync::watch::Sender<WorkerActivity>,
    /// Test seams: fail the next joiner-thread starts, hold a worker thread's
    /// handle back from its record, and run code inside a join's window.
    #[cfg(test)]
    seams: workers::Seams,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the core owns: its children, its workers and the fence in front of
/// them. Cheap to clone; every clone is the same ledger.
#[derive(Debug, Clone)]
pub struct SpawnLedger {
    inner: Arc<Inner>,
}

impl Default for SpawnLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl SpawnLedger {
    /// An empty ledger with an open fence.
    pub fn new() -> Self {
        let (children_activity, _) = tokio::sync::watch::channel(ChildActivity::default());
        let (workers_activity, _) = tokio::sync::watch::channel(WorkerActivity::default());
        Self {
            inner: Arc::new(Inner {
                fence: RwLock::new(false),
                fenced: AtomicBool::new(false),
                fence_state: tokio::sync::watch::channel(FenceState::Open).0,
                children: Mutex::new(children::Table::default()),
                children_activity,
                workers: Mutex::new(workers::Table::default()),
                workers_activity,
                #[cfg(test)]
                seams: workers::Seams::default(),
            }),
        }
    }

    /// Begin a spawn. Every child and worker of one launch is started and
    /// registered within the returned [`Admission`], which the final fence
    /// waits for.
    ///
    /// The admission holds a lock: use it in a synchronous section and drop it
    /// before any `.await` (it is not `Send`, so an async task cannot hold it
    /// across one).
    ///
    /// # Errors
    /// [`SpawnError::Fenced`] once [`close_fence`](Self::close_fence) has run.
    pub fn admit(&self) -> Result<Admission<'_>, SpawnError> {
        let guard = self
            .inner
            .fence
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        if *guard {
            return Err(SpawnError::Fenced);
        }
        Ok(Admission {
            managed: Some((self, guard)),
        })
    }

    /// [`admit`](Self::admit) for a site whose ledger is optional.
    ///
    /// # Errors
    /// [`SpawnError::Fenced`] when the ledger's fence is closed.
    pub fn admit_optional(ledger: Option<&Self>) -> Result<Admission<'_>, SpawnError> {
        ledger.map_or_else(|| Ok(Admission::unmanaged()), Self::admit)
    }

    /// Close the spawn fence, for good.
    ///
    /// Waits for the admissions in flight, so every launch that began before
    /// this returns is in the ledger, and every later [`admit`](Self::admit)
    /// fails. There is no reopening: a restart that fails after this stays
    /// failed closed. Blocks only for the spawns in flight (each is a short
    /// synchronous section); calling it again is harmless.
    pub fn close_fence(&self) {
        self.inner.fence_state.send_modify(|state| {
            if *state == FenceState::Open {
                *state = FenceState::Closing;
            }
        });
        let mut closed = self
            .inner
            .fence
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        *closed = true;
        self.inner.fenced.store(true, Ordering::Release);
        self.inner.fence_state.send_replace(FenceState::Closed);
    }

    /// A watch on the fence, for event-driven waits.
    pub fn watch_fence(&self) -> tokio::sync::watch::Receiver<FenceState> {
        self.inner.fence_state.subscribe()
    }

    /// Whether [`close_fence`](Self::close_fence) has completed.
    pub fn is_fenced(&self) -> bool {
        self.inner.fenced.load(Ordering::Acquire)
    }
}

/// A spawn in progress: the fence cannot close until this is dropped.
///
/// Created by [`SpawnLedger::admit`]. [`Admission::unmanaged`] is the same
/// interface for a site without a ledger.
pub struct Admission<'a> {
    managed: Option<(&'a SpawnLedger, RwLockReadGuard<'a, bool>)>,
}

impl std::fmt::Debug for Admission<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Admission")
            .field("managed", &self.managed.is_some())
            .finish()
    }
}

impl Admission<'static> {
    /// No ledger: spawns are not registered and no fence applies.
    pub const fn unmanaged() -> Self {
        Self { managed: None }
    }
}

impl Admission<'_> {
    /// The ledger this admission belongs to, if any.
    pub fn ledger(&self) -> Option<&SpawnLedger> {
        self.managed.as_ref().map(|(ledger, _)| *ledger)
    }

    /// Spawn `command` as a [`GroupChild`] and register it.
    ///
    /// `command` must carry the launch policy, as for [`GroupChild::spawn`].
    /// `label` is a short name for diagnostics; never put a secret in it.
    ///
    /// # Errors
    /// The spawn error of [`GroupChild::spawn`].
    pub fn spawn_group(
        &self,
        kind: ChildKind,
        label: Option<&str>,
        command: &mut tokio::process::Command,
    ) -> io::Result<GroupChild> {
        let mut child = GroupChild::spawn(command)?;
        if let Some(entry) = self.register_group(kind, label, child.group()) {
            child.attach(entry);
        }
        Ok(child)
    }

    /// Register a child some other owner started inside this admission.
    ///
    /// `group` is the group of a child started with the launch policy. The
    /// owner calls [`ChildEntry::reaped`] once it has reaped the leader; a
    /// dropped entry means the owner gave the child up without reaping it.
    /// `None` without a ledger.
    pub fn register_group(
        &self,
        kind: ChildKind,
        label: Option<&str>,
        group: &ProcessGroup,
    ) -> Option<ChildEntry> {
        let ledger = self.ledger()?;
        Some(children::register(&ledger.inner, kind, label, group))
    }

    /// Spawn a monitoring task the ledger can join.
    ///
    /// Without a ledger this is a plain `tokio::spawn`. Must run inside a Tokio
    /// runtime.
    pub fn spawn_worker<F>(&self, name: &'static str, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        match &self.managed {
            Some((ledger, _)) => workers::spawn_task(&ledger.inner, name, future),
            None => drop(tokio::spawn(future)),
        }
    }

    /// Spawn a blocking worker on its own thread the ledger can join.
    ///
    /// Without a ledger this is a plain detached thread.
    ///
    /// # Errors
    /// The error of starting the thread.
    pub fn spawn_worker_thread<F>(&self, name: &'static str, work: F) -> io::Result<()>
    where
        F: FnOnce() + Send + 'static,
    {
        match &self.managed {
            Some((ledger, _)) => workers::spawn_thread(&ledger.inner, name, work),
            None => std::thread::Builder::new()
                .name(name.to_owned())
                .spawn(work)
                .map(drop),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
