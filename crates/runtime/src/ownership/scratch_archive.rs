//! UI and host commands send IDs only. Saved-state IO and fenced rename run here.
use super::{Registry, RegistryError};
use crate::scratch::ThreadScratch;
use std::{
    collections::HashSet,
    io,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, mpsc},
    thread::JoinHandle,
    time::Duration,
};
/// Saved shared-sidebar archive state, queried only by the archive worker.
pub type ScratchArchiveChecker = dyn Fn(&str) -> io::Result<bool> + Send + Sync;
type Checker = Arc<ScratchArchiveChecker>;
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
#[derive(Default)]
struct State {
    pending: HashSet<String>,
    checker: Option<Checker>,
    stopped: bool,
    #[cfg(any(test, feature = "idle-test-support"))]
    barriers: Vec<mpsc::Sender<()>>,
}
pub(super) struct ArchiveService {
    state: Arc<Mutex<State>>,
    wake: mpsc::SyncSender<()>,
    join: Mutex<Option<JoinHandle<()>>>,
}
impl ArchiveService {
    pub(super) fn new(db: PathBuf, owner: String, scratch: Arc<ThreadScratch>) -> io::Result<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let (wake, rx) = mpsc::sync_channel(1);
        scratch.set_archive_wake(wake.clone());
        let shared = Arc::clone(&state);
        let join = std::thread::Builder::new()
            .name("evorch-scratch-archive".into())
            .spawn(move || serve(db, owner, scratch, shared, rx, Duration::from_millis(100)))?;
        Ok(Self {
            state,
            wake,
            join: Mutex::new(Some(join)),
        })
    }
    pub(super) fn set_checker(&self, checker: Checker) {
        lock(&self.state).checker = Some(checker);
        let _ = self.wake.try_send(());
    }
    pub(super) fn hint(&self, thread: &str) {
        let mut state = lock(&self.state);
        if state.stopped {
            return;
        }
        state.pending.insert(thread.into());
        drop(state);
        let _ = self.wake.try_send(());
    }
    pub(super) fn stop(&self) -> io::Result<()> {
        let mut join = lock(&self.join);
        lock(&self.state).stopped = true;
        let _ = self.wake.try_send(());
        if let Some(join) = join.take() {
            join.join()
                .map_err(|_| io::Error::other("scratch archive worker panicked"))?;
        }
        Ok(())
    }
    #[cfg(any(test, feature = "idle-test-support"))]
    pub(super) fn barrier(&self) -> mpsc::Receiver<()> {
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

#[cfg(test)]
#[path = "scratch_archive_tests.rs"]
mod tests;

fn attempt(
    db: &std::path::Path,
    owner: &str,
    scratch: &ThreadScratch,
    thread: &str,
    checker: &Checker,
) -> Result<bool, RegistryError> {
    // Busy users short-circuit BEFORE reading the sidebar. No await under either gate.
    let Some((epoch, lease)) = scratch.archive_snapshot(thread) else {
        return Ok(false);
    };
    if !checker(thread)? {
        return Ok(true);
    }
    if lease.owner_id != owner {
        return Ok(false);
    }
    let result = Registry::open_existing(db)?.archive_scratch(thread, &lease, || {
        scratch.detach_archived(thread, epoch, &lease)
    })?;
    // Both the registry write reservation and ThreadScratch mutex are now released.
    if result == Some(true) {
        scratch.wake_trash()?;
    }
    Ok(result.is_some())
}
fn serve(
    db: PathBuf,
    owner: String,
    scratch: Arc<ThreadScratch>,
    state: Arc<Mutex<State>>,
    rx: mpsc::Receiver<()>,
    backoff: Duration,
) {
    let mut retry = false;
    loop {
        if retry {
            match rx.recv_timeout(backoff) {
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
        let (pending, checker) = {
            let mut s = lock(&state);
            if s.stopped {
                return;
            }
            #[cfg(any(test, feature = "idle-test-support"))]
            {
                barriers = std::mem::take(&mut s.barriers);
            }
            // Move hints out before IO. A new concurrent hint must survive this pass.
            (std::mem::take(&mut s.pending), s.checker.clone())
        };
        if let Some(checker) = checker {
            for thread in pending {
                if lock(&state).stopped {
                    return;
                }
                let retain = match attempt(&db, &owner, &scratch, &thread, &checker) {
                    Ok(done) => !done,
                    Err(error) if error.is_contention() => {
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
