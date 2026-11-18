//! The registration state machine of a reindex request (ADR 0041 R5, ADR 0045 §10).
//!
//! A registration is a sequence of bounded writer commands (discovery batches, freeze
//! batches, policy-gap batches, settle windows, one publish), each a short transaction. The caller
//! drives it with `StorageHandle::advance_thread_index_registration` and may check its
//! own stop and cancel conditions between steps; the writer serves its other commands
//! between them. A registration that is not driven to the end is closed with
//! `abort_thread_index_registration`; one left behind by a dead process is closed by the
//! start-up sweep.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use super::requests::{FreezeCursor, SettleCursor};
use crate::thread_index::RequestSubmission;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Every run needs an index row before its targets are frozen.
    Discover,
    /// Freezing the targets, resumed after the cursor.
    Freeze(Option<FreezeCursor>),
    /// Closing the targets a disabled or paused index cannot work on.
    Gap,
    /// Settling the published requests the gap batches left without a pending target,
    /// one window of targets at a time (the first of `Registration::touched`).
    Settle(SettleCursor),
    /// The last short transaction.
    Publish,
}

/// A registration in progress. Opaque: only the steps of the writer move it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    pub(crate) request_id: String,
    pub(crate) phase: Phase,
    /// Published requests whose pending targets the gap batches closed: they end
    /// terminal, in bounded windows, before the publish.
    pub(crate) touched: Vec<String>,
    /// The gap batches ran under a gap policy and left no target pending, so the
    /// publish can end the request without looking at its targets.
    pub(crate) gap_closed: bool,
}

impl Registration {
    pub(crate) fn new(request_id: String) -> Self {
        Self {
            request_id,
            phase: Phase::Discover,
            touched: Vec::new(),
            gap_closed: false,
        }
    }

    /// The id the request has once it is published.
    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }
}

/// What one step of a registration achieved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationProgress {
    /// Another step is needed.
    InProgress,
    /// The request is pending (or terminal) and the owner is elected.
    Published(RequestSubmission),
}

type Live = Mutex<HashSet<(PathBuf, String)>>;

/// Registrations a writer of this process is running (one writer process per database,
/// ADR 0045): the start-up sweep of another storage of the same file must not close them.
fn live() -> &'static Live {
    static LIVE: OnceLock<Live> = OnceLock::new();
    LIVE.get_or_init(Live::default)
}

fn lock() -> std::sync::MutexGuard<'static, HashSet<(PathBuf, String)>> {
    live()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Creates a registration with `create` and marks it live as one step with respect to
/// [`sweep_dead`]: a sweep of another storage of the same file cannot run between the
/// commit of the `registering` row and the mark, and so cannot cancel a registration
/// that is about to run.
pub(super) fn begin_live<E>(
    db_path: &Path,
    create: impl FnOnce() -> Result<String, E>,
) -> Result<String, E> {
    let mut live = lock();
    let request_id = create()?;
    live.insert((db_path.to_owned(), request_id.clone()));
    Ok(request_id)
}

pub(super) fn unmark_live(db_path: &Path, request_id: &str) {
    lock().remove(&(db_path.to_owned(), request_id.to_owned()));
}

/// Runs `sweep` with the registrations this process still runs on `db_path`, while no
/// registration of this process can start or end.
pub(super) fn sweep_dead<T>(db_path: &Path, sweep: impl FnOnce(&[String]) -> T) -> T {
    let live = lock();
    let ids: Vec<String> = live
        .iter()
        .filter(|(path, _)| path == db_path)
        .map(|(_, id)| id.clone())
        .collect();
    sweep(&ids)
}

/// Whether the live set is locked right now (the exclusion tests look at it).
#[cfg(test)]
pub(super) fn is_locked() -> bool {
    matches!(live().try_lock(), Err(std::sync::TryLockError::WouldBlock))
}
