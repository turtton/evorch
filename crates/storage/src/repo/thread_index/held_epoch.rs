//! Changes of what a run's index holds, as seen by a multi-step terminalization.
//!
//! A target is `succeeded` while its run holds the frozen (revision, versions). The
//! re-check that terminalizes a request is one pass over its targets, and a pass made in
//! bounded windows is not atomic: a run read in an earlier window may lose its index
//! (retention, eviction, a newer commit, an accounting change) before the last window
//! ends the request. Each place that changes what a run holds calls
//! [`note_held_change`]; a pass remembers [`current`] when it starts and starts over
//! when it differs.
//!
//! The counter is process memory on purpose: the pass is transient too, a process that
//! dies restarts it from the first window, and one writer process owns a database
//! (ADR 0045), so every change this guards against goes through here. It is bumped only
//! when a `succeeded` target of the run exists (an index lookup), so the indexing of
//! runs no request has finished does not disturb a pass.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use rusqlite::Connection;

use crate::StorageError;

type Epochs = Mutex<BTreeMap<String, u64>>;

fn epochs() -> MutexGuard<'static, BTreeMap<String, u64>> {
    static EPOCHS: OnceLock<Epochs> = OnceLock::new();
    EPOCHS
        .get_or_init(Epochs::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn key(conn: &Connection) -> String {
    conn.path().unwrap_or_default().to_owned()
}

/// The index of `run_id` changed what it holds: a pass that read its targets earlier
/// must not terminalize on that reading.
pub(crate) fn note_held_change(conn: &Connection, run_id: &str) -> Result<(), StorageError> {
    let held: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM thread_index_request_targets \
         WHERE run_id = ?1 AND state = 'succeeded')",
        [run_id],
        |row| row.get(0),
    )?;
    if held {
        let mut epochs = epochs();
        *epochs.entry(key(conn)).or_default() += 1;
    }
    Ok(())
}

/// The epoch a pass compares against.
pub(crate) fn current(conn: &Connection) -> u64 {
    epochs().get(&key(conn)).copied().unwrap_or(0)
}
