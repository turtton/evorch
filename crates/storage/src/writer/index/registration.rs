//! The registration state machine of a reindex request (ADR 0041 R5, ADR 0045 §10).
//!
//! A registration is a sequence of bounded writer commands (discovery batches, freeze
//! batches, policy-gap batches, one publish), each a short transaction. The caller
//! drives it with `StorageHandle::advance_thread_index_registration` and may check its
//! own stop and cancel conditions between steps; the writer serves its other commands
//! between them. A registration that is not driven to the end is closed with
//! `abort_thread_index_registration`; one left behind by a dead process is closed by the
//! start-up sweep.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use super::requests::FreezeCursor;
use crate::thread_index::RequestSubmission;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Every run needs an index row before its targets are frozen.
    Discover,
    /// Freezing the targets, resumed after the cursor.
    Freeze(Option<FreezeCursor>),
    /// Closing the targets a disabled or paused index cannot work on.
    Gap,
    /// The last short transaction.
    Publish,
}

/// A registration in progress. Opaque: only the steps of the writer move it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    pub(crate) request_id: String,
    pub(crate) phase: Phase,
}

impl Registration {
    pub(crate) fn new(request_id: String) -> Self {
        Self {
            request_id,
            phase: Phase::Discover,
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

pub(super) fn mark_live(db_path: &Path, request_id: &str) {
    if let Ok(mut live) = live().lock() {
        live.insert((db_path.to_owned(), request_id.to_owned()));
    }
}

pub(super) fn unmark_live(db_path: &Path, request_id: &str) {
    if let Ok(mut live) = live().lock() {
        live.remove(&(db_path.to_owned(), request_id.to_owned()));
    }
}

pub(super) fn live_ids(db_path: &Path) -> Vec<String> {
    live()
        .lock()
        .map(|live| {
            live.iter()
                .filter(|(path, _)| path == db_path)
                .map(|(_, id)| id.clone())
                .collect()
        })
        .unwrap_or_default()
}
