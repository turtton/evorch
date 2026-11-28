//! UI and host commands send IDs only. Saved-state IO and fenced rename run here.
use crate::scratch::ThreadScratch;
use crate::thread_retention::TombstoneGuard;
use std::{
    collections::HashSet,
    io,
    sync::{Arc, Mutex, MutexGuard, mpsc},
    thread::JoinHandle,
    time::Duration,
};
/// Saved shared-sidebar archive state, queried only by the archive worker.
pub type ScratchArchiveChecker = dyn Fn(&str) -> io::Result<bool> + Send + Sync;
pub(crate) type Checker = Arc<ScratchArchiveChecker>;

/// Conservative archive authorization/rename supplied by the current runtime authority.
/// The worker owns scheduling and I/O isolation; this callback must revalidate
/// before mutating scratch. It does not authorize runs or manufacture permits.
pub type ArchiveAttempt = dyn Fn(
        &str,
        Option<&Arc<ScratchArchiveChecker>>,
        Option<&TombstoneGuard>,
    ) -> Result<bool, ArchiveError>
    + Send
    + Sync;

#[derive(Debug, thiserror::Error)]
#[error("{detail}")]
pub struct ArchiveError {
    pub(crate) contention: bool,
    pub(crate) detail: String,
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
#[derive(Default)]
pub(crate) struct State {
    pub(crate) pending: HashSet<String>,
    pub(crate) checker: Option<Checker>,
    pub(crate) tombstones: Option<crate::thread_retention::TombstoneGuard>,
    pub(crate) stopped: bool,
    #[cfg(any(test, feature = "idle-test-support"))]
    pub(crate) barriers: Vec<mpsc::Sender<()>>,
}
pub struct ArchiveService {
    state: Arc<Mutex<State>>,
    wake: mpsc::SyncSender<()>,
    join: Mutex<Option<JoinHandle<()>>>,
}
impl ArchiveService {
    pub fn new(scratch: &ThreadScratch, attempt: Arc<ArchiveAttempt>) -> io::Result<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let (wake, rx) = mpsc::sync_channel(1);
        scratch.set_archive_wake(wake.clone());
        let shared = Arc::clone(&state);
        let join = std::thread::Builder::new()
            .name("evorch-scratch-archive".into())
            .spawn(move || {
                serve(attempt, shared, rx, |rx| {
                    rx.recv_timeout(Duration::from_millis(100))
                })
            })?;
        Ok(Self {
            state,
            wake,
            join: Mutex::new(Some(join)),
        })
    }
    pub fn set_checker(&self, checker: Arc<ScratchArchiveChecker>) {
        lock(&self.state).checker = Some(checker);
        let _ = self.wake.try_send(());
    }
    pub fn set_guard(&self, guard: crate::thread_retention::TombstoneGuard) {
        lock(&self.state).tombstones = Some(guard);
        let _ = self.wake.try_send(());
    }
    pub fn hint(&self, thread: &str) {
        let mut state = lock(&self.state);
        if state.stopped {
            return;
        }
        state.pending.insert(thread.into());
        drop(state);
        let _ = self.wake.try_send(());
    }
    /// Stop/join archive I/O after producers and runtime users have drained.
    pub fn stop(&self) -> io::Result<()> {
        let mut join = lock(&self.join);
        lock(&self.state).stopped = true;
        let _ = self.wake.try_send(());
        if let Some(join) = join.take() {
            join.join()
                .map_err(|_| io::Error::other("scratch archive worker panicked"))?;
        }
        Ok(())
    }
    /// Stop/join archive and the process scratch collector before storage/leases.
    pub fn stop_cleanup(&self) -> io::Result<()> {
        let archive = self.stop();
        let cleanup = sandbox::scratch::stop_scratch_cleanup();
        archive.and(cleanup)
    }
    #[cfg(any(test, feature = "idle-test-support"))]
    pub fn barrier(&self) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        let mut s = lock(&self.state);
        if s.stopped {
            drop(tx);
            return rx;
        }
        s.barriers.push(tx);
        drop(s);
        let _ = self.wake.try_send(());
        rx
    }
}
impl Drop for ArchiveService {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

pub(crate) fn serve(
    attempt: Arc<ArchiveAttempt>,
    state: Arc<Mutex<State>>,
    rx: mpsc::Receiver<()>,
    backoff: impl Fn(&mpsc::Receiver<()>) -> Result<(), mpsc::RecvTimeoutError>,
) {
    let mut retry = false;
    loop {
        if retry {
            match backoff(&rx) {
                Ok(()) => retry = false,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        } else if rx.recv().is_err() {
            return;
        }
        let backoff_attempt = retry;
        retry = false;
        #[cfg(any(test, feature = "idle-test-support"))]
        let barriers;
        let (pending, checker, tombstones) = {
            let mut s = lock(&state);
            if s.stopped {
                return;
            }
            #[cfg(any(test, feature = "idle-test-support"))]
            {
                barriers = std::mem::take(&mut s.barriers);
            }
            // Move hints out before IO. A new concurrent hint must survive this pass.
            (
                std::mem::take(&mut s.pending),
                s.checker.clone(),
                s.tombstones.clone(),
            )
        };
        if checker.is_some() || tombstones.is_some() {
            for thread in pending {
                if lock(&state).stopped {
                    return;
                }
                let retain = match attempt(&thread, checker.as_ref(), tombstones.as_ref()) {
                    Ok(done) => !done,
                    Err(error) if error.contention => {
                        retry |= !backoff_attempt;
                        true
                    }
                    Err(error) => {
                        tracing::warn!(%thread,%error,"archived scratch retained");
                        true
                    }
                };
                if retain {
                    lock(&state).pending.insert(thread);
                }
            }
        } else {
            lock(&state).pending.extend(pending);
        }
        #[cfg(any(test, feature = "idle-test-support"))]
        for barrier in barriers {
            let _ = barrier.send(());
        }
    }
}

/// Construct the real archive authority from the existing root writer lease and
/// bidirectional retention binding check. Unbound is safe under the root writer
/// lease; mismatched binding retains scratch and is diagnosed.
/// The captured Arc keeps writer authority alive through the last worker attempt.
pub fn authority(
    storage: storage::StorageConfig,
    root: std::path::PathBuf,
    sidebar: std::path::PathBuf,
    leases: Arc<storage::maintenance::writer_leases::WriterLeases>,
    scratch: Arc<ThreadScratch>,
) -> Arc<ArchiveAttempt> {
    let context = ScratchAuthority {
        leases,
        root,
        sidebar,
    };
    Arc::new(move |thread, checker, tombstones| {
        let attempt = || -> Result<bool, Box<dyn std::error::Error>> {
            if !context.validate(&storage.db_path)? {
                return Ok(false);
            }
            let Some(epoch) = scratch.archive_snapshot(thread) else {
                return Ok(false);
            };
            let deleted = tombstones.is_some_and(|guard| guard.check_thread(thread).is_err());
            if !deleted {
                let Some(checker) = checker else {
                    return Ok(false);
                };
                if !checker(thread)? {
                    return Ok(true);
                }
            }
            // The epoch and every run/shell user are checked again under the
            // publication gate, immediately before detaching this one namespace.
            let result = scratch.detach_archived(thread, epoch)?;
            if result == Some(true) {
                scratch.wake_trash()?;
            }
            Ok(result.is_some())
        };
        attempt().map_err(|error| ArchiveError {
            contention: false,
            detail: error.to_string(),
        })
    })
}

/// Process-local composition of existing leases and canonical binding paths.
/// This is not a registry, nor serialized execution authority.
pub(crate) struct ScratchAuthority {
    pub(crate) leases: Arc<storage::maintenance::writer_leases::WriterLeases>,
    pub(crate) root: std::path::PathBuf,
    pub(crate) sidebar: std::path::PathBuf,
}
impl ScratchAuthority {
    pub(crate) fn validate(&self, db_path: &std::path::Path) -> io::Result<bool> {
        let Some(authority) = self.leases.authority() else {
            return Ok(false);
        };
        authority
            .validate(db_path, &self.root)
            .map_err(io::Error::other)?;
        let db = storage::Database::open_read_only(db_path).map_err(io::Error::other)?;
        let binding = crate::thread_retention::check_binding(
            &db,
            crate::thread_retention::BindingPaths {
                ownership_root: &self.root,
                sidebar_path: &self.sidebar,
            },
        )
        .map_err(io::Error::other)?;
        match binding {
            crate::thread_retention::BindingStatus::Mismatch(reason) => {
                tracing::warn!(%reason, "scratch cleanup retained: retention binding mismatch");
                Ok(false)
            }
            crate::thread_retention::BindingStatus::Bound
            | crate::thread_retention::BindingStatus::Unbound => Ok(true),
        }
    }
}
