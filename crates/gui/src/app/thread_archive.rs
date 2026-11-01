use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use workspace_ui::{LineageIndex, ThreadError, ThreadId, ThreadRecord};

use super::thread_retention::now_unix_seconds;
use super::{WorkbenchError, WorkbenchState};
use crate::model::tasks::AgentRunSource;
use crate::panes::sidebar::threads::{
    family_has_running_runs, thread_family, threads_with_active_runs,
};

/// What an archive or restore did to the sidebar.
struct ArchiveChange {
    archived: bool,
    family: BTreeSet<ThreadId>,
}

impl<S: AgentRunSource> WorkbenchState<S> {
    pub fn toggle_archive(&mut self, thread_id: ThreadId) -> Result<(), WorkbenchError> {
        let thread = self
            .sidebar
            .threads
            .iter()
            .find(|thread| thread.id == thread_id)
            .ok_or(ThreadError::UnknownThread)?;
        // The user acts on what this window shows; another window may have
        // changed the thread since, and the target stays what the user asked for.
        let archived = !thread.archived;
        let phases = self.phases.clone();
        let now = now_unix_seconds();
        // The same guards run again on the newest state under the save lock: a
        // family is archived as a unit, ignoring stale child actions and clicks on
        // a pinned root (even outside the sidebar UI), and an idle root must not
        // hide a running descendant. Restoring a family is always safe.
        let change = self
            .shared_sidebar_transact(|latest| {
                let threads = &mut latest.sidebar.threads;
                let lineage = LineageIndex::new(threads);
                let Some(thread) = threads.iter().find(|thread| thread.id == thread_id) else {
                    return Err(ThreadError::UnknownThread);
                };
                if lineage.display_parent(&thread_id).is_some() || (archived && thread.pinned) {
                    return Ok(None);
                }
                let family = thread_family(&lineage, &thread_id);
                if archived
                    && family_has_running_runs(&family, &threads_with_active_runs(threads, &phases))
                {
                    return Ok(None);
                }
                for thread in threads.iter_mut() {
                    if family.contains(&thread.id) {
                        thread.set_archived(archived, now);
                    }
                }
                Ok(Some(ArchiveChange { archived, family }))
            })
            .map_err(WorkbenchError::sidebar_busy)?;
        let Some(ArchiveChange { archived, family }) = change else {
            return Ok(());
        };
        self.refresh_retention();
        if archived
            && self
                .sidebar
                .active_thread
                .as_ref()
                .is_some_and(|active| family.contains(active))
        {
            let next = self.sidebar.selected_project.as_ref().and_then(|project| {
                ThreadRecord::partition_for_project(&self.sidebar.threads, project, &self.phases)
                    .0
                    .first()
                    .map(|thread| thread.id.clone())
            });
            if let Some(next) = next {
                self.switch_thread(next)?;
            } else {
                let previous = self.sidebar.active_thread.take();
                self.finish_thread_switch(previous);
            }
            return Ok(());
        }
        self.save_sidebar();
        Ok(())
    }

    /// Takes the archive and pin state of `source` from the newest sidebar, under the
    /// save lock, for `thread` (created from it).
    ///
    /// # Errors
    /// The sidebar could not be locked, read or written; nothing changed.
    fn reconcile_family_state(
        &mut self,
        thread: &ThreadId,
        source: &ThreadId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.shared_sidebar_transact(|latest| {
            let Some(source) = latest
                .sidebar
                .threads
                .iter()
                .find(|record| &record.id == source)
                .cloned()
            else {
                return Ok(());
            };
            let target = latest
                .sidebar
                .threads
                .iter_mut()
                .find(|record| &record.id == thread)
                .ok_or(ThreadError::UnknownThread)?;
            target.adopt_state_of(&source);
            Ok(())
        })
    }

    /// Remembers a thread whose archive and pin state could not be settled against
    /// the newest sidebar yet. Until it is, it is not mirrored, and
    /// [`Self::retry_pending_family_state`] settles it.
    pub(super) fn defer_family_state(&mut self, thread: ThreadId, source: ThreadId) {
        self.pending_family.threads.push((thread, source));
    }

    /// Settles the deferred threads once the sidebar can be locked again; tried at
    /// most once a second.
    pub(super) fn retry_pending_family_state(&mut self, now: Instant) {
        if self.pending_family.threads.is_empty()
            || self.pending_family.next_try.is_some_and(|next| now < next)
        {
            return;
        }
        self.pending_family.next_try = Some(now + FAMILY_RETRY);
        for (thread, source) in std::mem::take(&mut self.pending_family.threads) {
            if let Err(error) = self.reconcile_family_state(&thread, &source) {
                tracing::debug!(%error, %thread, "archive state still waits for the shared sidebar");
                self.pending_family.threads.push((thread, source));
            }
        }
    }
}

/// Threads created while the shared sidebar could not be locked.
#[derive(Default)]
pub(super) struct PendingFamily {
    pub(super) threads: Vec<(ThreadId, ThreadId)>,
    next_try: Option<Instant>,
}

const FAMILY_RETRY: Duration = Duration::from_secs(1);
