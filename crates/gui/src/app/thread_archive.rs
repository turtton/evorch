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

    /// Settles every thread flagged `state_pending` (see [`ThreadRecord`]) against
    /// its parent on the newest sidebar, under the save lock, in one transaction. The
    /// flag is saved with the record, so whichever window can lock the file settles
    /// it, also after a restart. Tried at most once a second.
    pub(super) fn retry_pending_family_state(&mut self, now: Instant) {
        if !self
            .sidebar
            .threads
            .iter()
            .any(|thread| thread.state_pending)
            || self.family_retry.0.is_some_and(|next| now < next)
        {
            return;
        }
        self.family_retry.0 = Some(now + FAMILY_RETRY);
        let settled = self.shared_sidebar_transact(|latest| {
            settle_pending_threads(&mut latest.sidebar.threads);
            Ok(())
        });
        if let Err(error) = settled {
            tracing::debug!(%error, "archive state still waits for the shared sidebar");
        }
    }
}

/// Gives each pending thread its parent's archive and pin state and clears the flag.
/// A thread whose parent is itself pending waits for it; one whose parent is gone has
/// nothing to take over and is settled as it is.
pub(super) fn settle_pending_threads(threads: &mut [ThreadRecord]) {
    loop {
        let mut progress = false;
        let pending: Vec<ThreadId> = threads
            .iter()
            .filter(|thread| thread.state_pending)
            .map(|thread| thread.id.clone())
            .collect();
        for id in pending {
            let parent = threads
                .iter()
                .find(|candidate| {
                    threads
                        .iter()
                        .find(|thread| thread.id == id)
                        .and_then(|thread| thread.parent_thread_id.as_ref())
                        == Some(&candidate.id)
                })
                .cloned();
            if parent.as_ref().is_some_and(|parent| parent.state_pending) {
                continue;
            }
            if let Some(thread) = threads.iter_mut().find(|thread| thread.id == id) {
                if let Some(parent) = &parent {
                    thread.adopt_state_of(parent);
                }
                thread.state_pending = false;
                progress = true;
            }
        }
        if !progress {
            return;
        }
    }
}

/// When the pending threads were last tried.
#[derive(Default)]
pub(super) struct FamilyRetry(Option<Instant>);

const FAMILY_RETRY: Duration = Duration::from_secs(1);
