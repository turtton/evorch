//! Where the local panes work: the directories the core reports for the selection,
//! as this machine sees them.
//!
//! The core client runs on the same host as the core, so a root the core names is a
//! directory here too. When it is not (a core on another machine, a deleted checkout),
//! the panes say so instead of running git or a shell somewhere else.
use std::path::{Path, PathBuf};

use workbench_protocol::ids::{ProjectId, ThreadId};

use crate::core_client::cache::Cache;
use crate::core_client::features::Selection;
use crate::core_client::features::workspace::SidebarModel;
use workbench_protocol::namespaces::workspace::TrustLevel;

/// One directory the core reported, checked against this machine.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LocalRoot {
    /// Nothing is selected that has a directory.
    #[default]
    None,
    /// The core has not answered yet.
    Pending,
    /// The core named a directory this machine does not have.
    Missing(PathBuf),
    Ready(PathBuf),
}

impl LocalRoot {
    fn local(reported: &str) -> Self {
        let path = PathBuf::from(reported);
        if path.is_absolute() && path.is_dir() {
            Self::Ready(path)
        } else {
            Self::Missing(path)
        }
    }

    pub fn ready(&self) -> Option<&Path> {
        match self {
            Self::Ready(path) => Some(path),
            _ => None,
        }
    }

    const fn known(&self) -> bool {
        matches!(self, Self::Ready(_) | Self::Missing(_))
    }
}

/// The directories of the selection the local panes work in.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LocalRoots {
    /// The selected project, which also keys the Terminal's shell.
    pub project: Option<ProjectId>,
    pub thread: Option<ThreadId>,
    /// The project's root: where its shell starts.
    pub project_root: LocalRoot,
    /// The selected thread's working tree (its active root, else the project's root):
    /// what the Diff shows.
    pub workspace: LocalRoot,
    /// The project's approved allowed directories that exist, as absolute paths: what
    /// the composer's `@` mentions list beside the project's own files.
    pub allowed: Vec<PathBuf>,
}

impl LocalRoots {
    /// The roots of `selection` from what the sidebar has read of the core.
    pub fn resolve(selection: &Selection, sidebar: Option<&SidebarModel>, cache: &Cache) -> Self {
        let access = selection
            .project
            .as_ref()
            .and_then(|project| sidebar?.access.get(project.as_str()));
        let project_root = match (&selection.project, access) {
            (None, _) => LocalRoot::None,
            (Some(_), None) => LocalRoot::Pending,
            (Some(_), Some(access)) => LocalRoot::local(&access.root),
        };
        let allowed = access
            .into_iter()
            .flat_map(|access| &access.allowed_directories)
            .filter(|directory| directory.trust == TrustLevel::Approved && directory.live)
            .map(|directory| PathBuf::from(&directory.path))
            .filter(|path| path.is_absolute())
            .collect();
        let workspace = match &selection.thread {
            // A new conversation is not in the core yet: it works in the project.
            Some(thread) if cache.threads.contains_key(thread.as_str()) => {
                match sidebar.and_then(|sidebar| sidebar.details.get(thread.as_str())) {
                    // A worktree missing here stays missing: the project's root is
                    // another tree, and its diff would not be this thread's.
                    Some(detail) => detail
                        .active_root
                        .as_deref()
                        .map_or_else(|| project_root.clone(), LocalRoot::local),
                    None => LocalRoot::Pending,
                }
            }
            _ => project_root.clone(),
        };
        Self {
            project: selection.project.clone(),
            thread: selection.thread.clone(),
            project_root,
            workspace,
            allowed,
        }
    }

    /// Keeps what was known of the same selection while it is being read again: a new
    /// session clears the sidebar until the core answers once more.
    #[must_use]
    pub fn keep_known(mut self, previous: &Self) -> Self {
        if self.project != previous.project || self.thread != previous.thread {
            return self;
        }
        if self.project_root == LocalRoot::Pending && previous.project_root.known() {
            self.allowed.clone_from(&previous.allowed);
        }
        for (fresh, known) in [
            (&mut self.project_root, &previous.project_root),
            (&mut self.workspace, &previous.workspace),
        ] {
            if *fresh == LocalRoot::Pending && known.known() {
                *fresh = known.clone();
            }
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use workbench_protocol::namespaces::workspace::{
        AllowedDirectoryView, ProjectAccessView, ThreadDetail,
    };
    use workbench_protocol::views::ThreadSummary;
    use workbench_protocol::wire::Revision;

    use super::*;

    struct World {
        project: tempfile::TempDir,
        worktree: tempfile::TempDir,
        sidebar: SidebarModel,
        cache: Cache,
    }

    fn selection(project: Option<&str>, thread: Option<&str>) -> Selection {
        Selection {
            project: project.map(|id| ProjectId::new(id).unwrap()),
            thread: thread.map(|id| ThreadId::new(id).unwrap()),
        }
    }

    fn detail(thread: &str, active_root: Option<&Path>) -> ThreadDetail {
        ThreadDetail {
            thread: ThreadId::new(thread).unwrap(),
            project: ProjectId::new("p").unwrap(),
            title: thread.into(),
            family: None,
            parent_thread: None,
            lineage: None,
            root_run: None,
            active_root: active_root.map(|root| root.to_string_lossy().into_owned()),
            model_profile: None,
            chat_role: None,
            extra_read_dirs: Vec::new(),
            archived: false,
            pinned: false,
            revision: Revision(1),
        }
    }

    /// Project `p` with threads `in-project` (no active root), `in-worktree` (its own
    /// root) and `unread` (detail not read); the access of `p` is read.
    fn world() -> World {
        let project = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let mut sidebar = SidebarModel::default();
        sidebar.access.insert(
            "p".into(),
            ProjectAccessView {
                project: ProjectId::new("p").unwrap(),
                name: "p".into(),
                root: project.path().to_string_lossy().into_owned(),
                trust: TrustLevel::Approved,
                allowed_directories: Vec::new(),
                revision: Revision(1),
            },
        );
        let mut cache = Cache::default();
        for thread in ["in-project", "in-worktree", "unread"] {
            let summary: ThreadSummary = serde_json::from_value(json!({
                "id": thread, "project": "p", "title": thread, "state": "idle",
                "revision": "1", "updated_at": "1",
            }))
            .unwrap();
            cache.threads.insert(thread.into(), summary);
        }
        sidebar
            .details
            .insert("in-project".into(), detail("in-project", None));
        sidebar.details.insert(
            "in-worktree".into(),
            detail("in-worktree", Some(worktree.path())),
        );
        World {
            project,
            worktree,
            sidebar,
            cache,
        }
    }

    impl World {
        fn resolve(&self, project: Option<&str>, thread: Option<&str>) -> LocalRoots {
            LocalRoots::resolve(
                &selection(project, thread),
                Some(&self.sidebar),
                &self.cache,
            )
        }
    }

    // Given: a thread whose detail names its own working tree
    // When: it is selected
    // Then: the diff works there and the shell in the project's root
    #[test]
    fn a_thread_with_an_active_root_works_in_it() {
        let world = world();
        let roots = world.resolve(Some("p"), Some("in-worktree"));
        assert_eq!(
            roots.workspace,
            LocalRoot::Ready(world.worktree.path().into())
        );
        assert_eq!(
            roots.project_root,
            LocalRoot::Ready(world.project.path().into())
        );
    }

    // Given: a thread without an active root, and a new conversation not in the core yet
    // When: either is selected
    // Then: both work in the project's root
    #[test]
    fn a_thread_without_an_active_root_works_in_the_project() {
        let world = world();
        for thread in ["in-project", "new-conversation"] {
            let roots = world.resolve(Some("p"), Some(thread));
            assert_eq!(
                roots.workspace,
                LocalRoot::Ready(world.project.path().into()),
                "{thread}"
            );
        }
    }

    // Given: a thread whose detail is not read, and a project whose access is not read
    // When: either is selected
    // Then: what is not read yet is pending
    #[test]
    fn what_the_core_has_not_answered_is_pending() {
        let world = world();
        let roots = world.resolve(Some("p"), Some("unread"));
        assert_eq!(roots.workspace, LocalRoot::Pending);
        assert!(roots.project_root.ready().is_some());

        let roots = world.resolve(Some("other"), None);
        assert_eq!(
            (roots.project_root, roots.workspace),
            (LocalRoot::Pending, LocalRoot::Pending)
        );

        let roots = LocalRoots::resolve(&selection(Some("p"), None), None, &world.cache);
        assert_eq!(roots.workspace, LocalRoot::Pending);
    }

    // Given: a project root and a worktree this machine does not have, and a relative root
    // When: they are resolved
    // Then: each is missing, and a missing worktree does not fall back to the project
    #[test]
    fn a_root_this_machine_does_not_have_is_missing() {
        let mut world = world();
        let gone = world.project.path().join("gone");
        world
            .sidebar
            .details
            .insert("in-worktree".into(), detail("in-worktree", Some(&gone)));
        let roots = world.resolve(Some("p"), Some("in-worktree"));
        assert_eq!(roots.workspace, LocalRoot::Missing(gone.clone()));

        for root in [gone.to_string_lossy().into_owned(), "relative/root".into()] {
            world.sidebar.access.get_mut("p").unwrap().root = root.clone();
            let roots = world.resolve(Some("p"), None);
            assert_eq!(roots.project_root, LocalRoot::Missing(root.into()));
        }
    }

    #[test]
    fn no_project_has_no_root() {
        let world = world();
        let roots = world.resolve(None, None);
        assert_eq!(
            (roots.project_root, roots.workspace),
            (LocalRoot::None, LocalRoot::None)
        );
    }

    // Given: the roots of a selection were known
    // When: a new session reads them again, for the same and for another selection
    // Then: the same selection keeps them meanwhile; another one does not
    #[test]
    fn known_roots_survive_a_reread_of_the_same_selection_only() {
        let world = world();
        let known = world.resolve(Some("p"), Some("in-worktree"));
        let rereading = |thread| {
            LocalRoots::resolve(
                &selection(Some("p"), Some(thread)),
                Some(&SidebarModel::default()),
                &world.cache,
            )
        };
        assert_eq!(rereading("in-worktree").keep_known(&known), known);

        let other = rereading("in-project").keep_known(&known);
        assert_eq!(
            (other.project_root, other.workspace),
            (LocalRoot::Pending, LocalRoot::Pending)
        );
    }

    // Given: the project's allowed directories: approved and live, approved but gone,
    //        not approved, and a relative path
    // When: the project is resolved, then read again by a new session
    // Then: only the approved, live, absolute one is listed, and the reread keeps it
    #[test]
    fn only_approved_live_allowed_directories_are_listed() {
        let mut world = world();
        let shared = world.worktree.path().to_string_lossy().into_owned();
        let directory = |path: &str, trust, live| AllowedDirectoryView {
            path: path.into(),
            trust,
            live,
        };
        world
            .sidebar
            .access
            .get_mut("p")
            .unwrap()
            .allowed_directories = vec![
            directory(&shared, TrustLevel::Approved, true),
            directory("/gone", TrustLevel::Approved, false),
            directory("/unapproved", TrustLevel::Unapproved, true),
            directory("relative", TrustLevel::Approved, true),
        ];
        let known = world.resolve(Some("p"), None);
        assert_eq!(known.allowed, [PathBuf::from(&shared)]);

        let reread = LocalRoots::resolve(
            &selection(Some("p"), None),
            Some(&SidebarModel::default()),
            &world.cache,
        )
        .keep_known(&known);
        assert_eq!(reread.allowed, known.allowed);
    }
}
