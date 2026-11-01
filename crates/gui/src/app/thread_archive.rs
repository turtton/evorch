use std::collections::BTreeSet;

use workspace_ui::{ThreadError, ThreadId, ThreadRecord};

use super::thread_retention::now_unix_seconds;
use super::{WorkbenchError, WorkbenchState};
use crate::model::tasks::AgentRunSource;
use crate::panes::sidebar::threads::{family_has_running_runs, threads_with_active_runs};

/// What an archive or restore did to the sidebar.
struct ArchiveChange {
    archived: bool,
    family: BTreeSet<ThreadId>,
}

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Archives or restores the whole family of the thread (its versions, forks and
    /// escalations). The edit is made under the sidebar save lock on the newest state,
    /// and fails, changing nothing, when that cannot be locked or read.
    pub fn toggle_archive(&mut self, thread_id: ThreadId) -> Result<(), WorkbenchError> {
        let thread = self
            .sidebar
            .threads
            .iter()
            .find(|thread| thread.id == thread_id)
            .ok_or(ThreadError::UnknownThread)?;
        // The user acts on what this window shows; another window may have
        // changed the family since, and the target stays what the user asked for.
        let archived = !thread.archived;
        let phases = self.phases.clone();
        let now = now_unix_seconds();
        // The same guards run again on the newest state: a family is archived from
        // its displayed root (stale child actions and clicks on a pinned family are
        // ignored, also outside the sidebar UI), and an idle root must not hide a
        // running descendant. Restoring a family is always safe.
        let change = self
            .shared_sidebar_transact(|latest| {
                let sidebar = &mut latest.sidebar;
                if !sidebar.threads.iter().any(|thread| thread.id == thread_id) {
                    return Err(ThreadError::UnknownThread);
                }
                let key = sidebar.family_key(&thread_id)?;
                let family: BTreeSet<ThreadId> = sidebar.family_members(&key).into_iter().collect();
                let states = sidebar.resolve_states();
                let pinned = states.get(&thread_id).is_some_and(|state| state.pinned);
                if workspace_ui::display_parent(&sidebar.threads, &thread_id).is_some()
                    || (archived && pinned)
                {
                    return Ok(None);
                }
                if archived
                    && family_has_running_runs(
                        &family,
                        &threads_with_active_runs(&sidebar.threads, &phases),
                    )
                {
                    return Ok(None);
                }
                sidebar.set_family_archived(&thread_id, archived, now)?;
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
}
