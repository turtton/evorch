//! Transitional owner-specific archive validation; scheduling lives in scratch.
use super::{Registry, RegistryError};
use crate::scratch::{
    ThreadScratch,
    archive::{ArchiveAttempt, ArchiveError, ScratchArchiveChecker},
};
use std::{path::PathBuf, sync::Arc};
type Checker = Arc<ScratchArchiveChecker>;

pub(super) fn authority(
    db: PathBuf,
    owner: String,
    scratch: Arc<ThreadScratch>,
) -> Arc<ArchiveAttempt> {
    Arc::new(move |thread, checker, tombstones| {
        attempt(&db, &owner, &scratch, thread, checker, tombstones).map_err(|error| ArchiveError {
            contention: error.is_contention(),
            detail: error.to_string(),
        })
    })
}

#[cfg(test)]
#[path = "scratch_archive_tests.rs"]
mod tests;

fn attempt(
    db: &std::path::Path,
    owner: &str,
    scratch: &ThreadScratch,
    thread: &str,
    checker: Option<&Checker>,
    tombstones: Option<&crate::thread_retention::TombstoneGuard>,
) -> Result<bool, RegistryError> {
    let deleted = tombstones.is_some_and(|guard| guard.check_thread(thread).is_err());
    // Busy users short-circuit BEFORE reading the sidebar. No await under either gate.
    let Some((epoch, lease)) = scratch.archive_snapshot(thread) else {
        // Deleted hints have a durable cursor retry; do not retain every tombstone
        // in this process when no lease exists (or while a user is still active).
        return Ok(deleted);
    };
    if !deleted {
        let Some(checker) = checker else {
            return Ok(false);
        };
        if !checker(thread)? {
            return Ok(true);
        }
    }
    if lease.owner_id != owner {
        return Ok(deleted);
    }
    let result = Registry::open_existing(db)?.archive_scratch(thread, &lease, || {
        scratch.detach_archived(thread, epoch, &lease)
    })?;
    // Both the registry write reservation and ThreadScratch mutex are now released.
    if result == Some(true) {
        scratch.wake_trash()?;
    }
    // A tombstone cursor can resend every conservative skip, including foreign
    // ownership, a protective hold or an epoch race. Only archive hints need
    // in-memory retention until the last user releases its scratch.
    Ok(result.is_some() || deleted)
}
