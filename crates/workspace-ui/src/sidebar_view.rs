//! How this window shows the project list: sort order and collapsed projects.
//!
//! This is a per-user view preference kept with the local [`crate::Workspace`]
//! layout. It is deliberately not part of [`SidebarState`], which several
//! processes merge: one process sorting or collapsing must not rearrange
//! another's sidebar, and the canonical project order stays the registration order.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{ProjectId, ProjectRecord, SidebarState};

/// The order the sidebar lists projects in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectSort {
    /// The order projects were registered in (the stored order).
    #[default]
    Registration,
    /// Case-insensitive name, ascending.
    Name,
    /// Latest activity of a project's non-archived threads, newest first.
    RecentActivity,
}

/// The local, saved view state of the sidebar project list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidebarView {
    #[serde(default)]
    pub project_sort: ProjectSort,
    /// Projects whose thread list is folded away; every other project is open.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub collapsed_projects: BTreeSet<ProjectId>,
}

impl SidebarView {
    /// Whether the view equals what a fresh install shows (used to keep it out of saved files).
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn is_collapsed(&self, project: &ProjectId) -> bool {
        self.collapsed_projects.contains(project)
    }

    /// Folds or unfolds one project; returns whether the view changed.
    pub fn set_collapsed(&mut self, project: &ProjectId, collapsed: bool) -> bool {
        if collapsed {
            self.collapsed_projects.insert(project.clone())
        } else {
            self.collapsed_projects.remove(project)
        }
    }

    /// Folds or unfolds every project of `sidebar`; returns whether the view changed.
    pub fn set_all_collapsed(&mut self, sidebar: &SidebarState, collapsed: bool) -> bool {
        let before = self.collapsed_projects.clone();
        if collapsed {
            self.collapsed_projects
                .extend(sidebar.projects.iter().map(|project| project.id.clone()));
        } else {
            self.collapsed_projects.clear();
        }
        self.collapsed_projects != before
    }

    /// Whether at least one project of `sidebar` is open.
    pub fn any_open(&self, sidebar: &SidebarState) -> bool {
        sidebar
            .projects
            .iter()
            .any(|project| !self.is_collapsed(&project.id))
    }

    /// Forgets collapsed projects that no longer exist; returns whether anything was removed.
    pub fn retain_projects(&mut self, sidebar: &SidebarState) -> bool {
        let before = self.collapsed_projects.len();
        self.collapsed_projects
            .retain(|id| sidebar.projects.iter().any(|project| &project.id == id));
        self.collapsed_projects.len() != before
    }
}

/// The projects of `sidebar` in the order `sort` lists them. Only a reference
/// order is built: `sidebar.projects` and the threads' membership are untouched.
///
/// Ties fall back to the registration order, then to the project id.
pub fn ordered_projects(sidebar: &SidebarState, sort: ProjectSort) -> Vec<&ProjectRecord> {
    let mut projects: Vec<(usize, &ProjectRecord)> = sidebar.projects.iter().enumerate().collect();
    match sort {
        ProjectSort::Registration => {}
        ProjectSort::Name => projects.sort_by_cached_key(|(index, project)| {
            (project.name.to_lowercase(), *index, project.id.clone())
        }),
        ProjectSort::RecentActivity => {
            let latest = latest_activity(sidebar);
            // `None` (no live thread) sorts after every time once reversed.
            projects.sort_by_cached_key(|(index, project)| {
                (
                    std::cmp::Reverse(latest.get(&project.id).copied()),
                    *index,
                    project.id.clone(),
                )
            });
        }
    }
    projects.into_iter().map(|(_, project)| project).collect()
}

/// Latest activity of each project's non-archived threads, aggregated in one pass.
fn latest_activity(sidebar: &SidebarState) -> BTreeMap<&ProjectId, i64> {
    let mut latest = BTreeMap::new();
    for thread in sidebar.threads.iter().filter(|thread| !thread.archived) {
        let at = thread.last_activity();
        latest
            .entry(&thread.project_id)
            .and_modify(|current: &mut i64| *current = (*current).max(at))
            .or_insert(at);
    }
    latest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ThreadId;

    fn sidebar(names: &[(&str, &str)]) -> SidebarState {
        let mut sidebar = SidebarState::default();
        for (id, name) in names {
            let root = tempfile::tempdir().expect("temp dir");
            sidebar
                .add_project(ProjectId::new(*id), *name, root.path())
                .expect("project");
            // Keep the directory alive only while canonicalised; the record keeps the path.
            std::mem::forget(root);
        }
        sidebar
    }

    fn thread(sidebar: &mut SidebarState, id: &str, project: &str, at: i64, archived: bool) {
        sidebar
            .create_thread(ThreadId::new(id), ProjectId::new(project), id)
            .expect("thread");
        let record = sidebar.threads.last_mut().expect("created");
        record.created_at = at;
        record.updated_at = at;
        if archived {
            sidebar
                .set_family_archived(&ThreadId::new(id), true, at)
                .expect("archive");
        }
    }

    fn ids(projects: &[&ProjectRecord]) -> Vec<String> {
        projects
            .iter()
            .map(|project| project.id.to_string())
            .collect()
    }

    #[test]
    fn name_sort_ignores_case_and_keeps_registration_order_on_ties() {
        // Given: equal names in different case registered between other names.
        let sidebar = sidebar(&[("a", "beta"), ("b", "Alpha"), ("c", "alpha"), ("d", "Beta")]);
        // When: sorting by name.
        let order = ordered_projects(&sidebar, ProjectSort::Name);
        // Then: case is folded and the earlier registration wins a tie.
        assert_eq!(ids(&order), ["b", "c", "a", "d"]);
    }

    #[test]
    fn recent_activity_ignores_archived_threads_and_puts_idle_projects_last() {
        // Given: one project whose newest thread is archived, one idle, one active.
        let mut sidebar = sidebar(&[
            ("old", "old"),
            ("idle", "idle"),
            ("hot", "hot"),
            ("tie", "tie"),
        ]);
        thread(&mut sidebar, "t1", "old", 10, false);
        thread(&mut sidebar, "t2", "old", 99, true);
        thread(&mut sidebar, "t3", "hot", 50, false);
        thread(&mut sidebar, "t4", "tie", 50, false);
        // When: sorting by recent activity.
        let order = ordered_projects(&sidebar, ProjectSort::RecentActivity);
        // Then: newest first, equal times keep registration order, idle ends the list.
        assert_eq!(ids(&order), ["hot", "tie", "old", "idle"]);
    }

    #[test]
    fn registration_order_is_the_stored_order() {
        let sidebar = sidebar(&[("z", "z"), ("a", "a")]);
        assert_eq!(
            ids(&ordered_projects(&sidebar, ProjectSort::Registration)),
            ["z", "a"]
        );
    }

    #[test]
    fn view_serializes_as_nothing_when_default_and_prunes_missing_projects() {
        // Given: a collapsed project that is later removed.
        let mut sidebar = sidebar(&[("a", "a")]);
        let mut view = SidebarView::default();
        view.set_collapsed(&ProjectId::new("a"), true);
        view.set_collapsed(&ProjectId::new("gone"), true);
        // When: pruning against the current projects.
        assert!(view.retain_projects(&sidebar));
        // Then: only the live one stays, and unfolding everything restores the default.
        assert_eq!(view.collapsed_projects.len(), 1);
        assert!(!view.any_open(&sidebar));
        assert!(view.set_all_collapsed(&sidebar, false));
        assert!(view.is_default());
        sidebar.projects.clear();
        assert!(!view.any_open(&sidebar));
    }
}
