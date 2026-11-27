//! Wiring of archive-time thread scratch recovery.
//!
//! The host owns the scratch module's worker and injects saved shared-sidebar
//! state through [`install`]. The worker knows nothing about sidebar structure;
//! its ownership adapter still fences every rename against the current lease.
//!
//! The checker runs only on the runtime's archive worker thread. It reads the
//! shared sidebar file without its save lock ([`SidebarStore::read_unlocked`]:
//! the write is atomic, so a reader sees the old or the new file) and never
//! writes it. Anything it cannot establish keeps the scratch:
//!
//! - no shared sidebar configured, or the file cannot be read or parsed;
//! - the thread is not listed, or its family cannot be resolved.
//!
//! Only a resolved family decides: archived (collect) or not archived (the
//! hint is dropped). Attachments are never touched from here.
use runtime::scratch::archive::{ArchiveService, ScratchArchiveChecker};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use workspace_state::{SidebarStore, ThreadId};

/// The checker over the saved shared sidebar at `sidebar` (`None`: no shared
/// sidebar, every query keeps the scratch).
#[must_use]
pub fn sidebar_checker(sidebar: Option<PathBuf>) -> Arc<ScratchArchiveChecker> {
    Arc::new(move |thread: &str| {
        let Some(path) = &sidebar else {
            return Err(io::Error::other(
                "no shared sidebar to confirm the archive; keeping the scratch",
            ));
        };
        let document = SidebarStore::new(path.clone())
            .read_unlocked()
            .map_err(|error| io::Error::other(format!("shared sidebar unreadable: {error}")))?;
        match document
            .sidebar
            .resolve_states()
            .get(&ThreadId::new(thread))
        {
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "thread is not in the shared sidebar; keeping the scratch",
            )),
            Some(state) if !state.is_resolved() => Err(io::Error::other(
                "thread family is unresolved; keeping the scratch",
            )),
            Some(state) => Ok(state.archived),
        }
    })
}

/// Connects the host-owned archive worker to the saved shared sidebar. Call once
/// before archive hints can arrive.
pub fn install(archive: &ArchiveService, sidebar: Option<PathBuf>) {
    archive.set_checker(sidebar_checker(sidebar));
}
