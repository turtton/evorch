//! The thread tree of a project as the sidebar shows it, built from the core's
//! projection and the details the workspace feature read. Pure: nothing here draws.
//!
//! What the core does not say is approximated, never invented:
//! - a thread hangs under its lead when it is a lane (`ThreadSummary::lead_thread`),
//!   otherwise under `ThreadDetail::parent_thread`;
//! - a rewind replaces its source, so a version group shows one member: the one no
//!   other rewind points to (the newest of several);
//! - a thread whose detail is not read yet is a root titled by its summary.
use std::collections::{BTreeSet, HashMap, HashSet};

use workbench_protocol::{
    namespaces::workspace::LineageType,
    views::{
        ApprovalState, ProjectSummary, QuestionTarget, RunPhase, RunSummary, ThreadState,
        ThreadSummary,
    },
};
use workspace_ui::ProjectSort;

use super::SidebarModel;
use crate::core_client::cache::Cache;
use crate::panes::sidebar_widgets::{Attention, BranchKind, RowStatus, nest};

/// One displayed thread of a project and what its row shows.
#[derive(Clone, Copy, Debug)]
pub struct ThreadRow<'a> {
    pub summary: &'a ThreadSummary,
    /// The detail's title once read, the summary's until then.
    pub title: &'a str,
    /// Hangs under no displayed parent: only roots pin and archive.
    pub display_root: bool,
    /// Why it hangs under its parent; `None` for a root.
    pub kind: Option<BranchKind>,
    pub status: RowStatus,
    /// The thread runs: its root run works, or another run of it does (see [`RunState`]).
    pub running: bool,
    pub own: Attention,
    pub pinned: bool,
    pub archived: bool,
    /// A member of its family has a working run, so it cannot be archived.
    pub family_running: bool,
    /// Its detail is read. Until then whether it is pinned, archived or part of a
    /// running family is not known, so it is not offered for archiving.
    pub detail_known: bool,
    /// The working directory the thread runs in, when the core reported one.
    pub active_root: Option<&'a str>,
}

impl<'a> ThreadRow<'a> {
    pub fn id(&self) -> &'a str {
        self.summary.id.as_str()
    }
}

/// A project's displayed threads as `(row, depth)`, in tree order.
#[derive(Debug, Default)]
pub struct ProjectTree<'a> {
    /// Pinned roots first, then running families, then the newest.
    pub main: Vec<(ThreadRow<'a>, usize)>,
    /// Newest first.
    pub archived: Vec<(ThreadRow<'a>, usize)>,
}

/// A run that keeps its thread working, as the embedded workbench counts it: a run
/// waiting on an approval still runs there, one waiting for the user between turns
/// does not.
pub(super) fn run_is_working(phase: RunPhase) -> bool {
    matches!(
        phase,
        RunPhase::Pending | RunPhase::Running | RunPhase::WaitingForApproval
    )
}

/// What a thread's runs make of it, derived as the embedded workbench derives its
/// thread state. The core's `ThreadSummary::state` is not used for this: it says
/// Running while a run only waits for the user between turns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RunState {
    /// Its root run works, or another run of it does while the root has not stopped.
    Running,
    /// Its root run was cancelled.
    Stopped,
    /// Its root run failed.
    Failed,
    /// Nothing of it works: the root finished, waits for the user, or never ran.
    Settled,
}

/// The run of `thread` whose phase is the thread's: the detail's root run, or before
/// the detail is read, the thread's only run without a parent.
fn root_run<'a>(cache: &'a Cache, model: &SidebarModel, thread: &str) -> Option<&'a RunSummary> {
    if let Some(detail) = model.details.get(thread) {
        return cache.runs.get(detail.root_run.as_ref()?.as_str());
    }
    let mut roots = cache
        .runs
        .values()
        .filter(|run| run.thread.as_str() == thread && run.parent_run.is_none());
    let root = roots.next()?;
    roots.next().is_none().then_some(root)
}

/// How `thread`'s runs leave it (see [`RunState`]).
pub(super) fn run_state(cache: &Cache, model: &SidebarModel, thread: &str) -> RunState {
    let root = root_run(cache, model, thread);
    let state = match root.map(|run| run.phase) {
        Some(phase) if run_is_working(phase) => return RunState::Running,
        Some(RunPhase::Cancelled) => return RunState::Stopped,
        Some(RunPhase::Failed) => return RunState::Failed,
        _ => RunState::Settled,
    };
    // Subagents keep working while the root waits on them or between turns.
    let other_works = cache.runs.values().any(|run| {
        run.thread.as_str() == thread
            && root.is_none_or(|root| root.id != run.id)
            && run_is_working(run.phase)
    });
    if other_works {
        RunState::Running
    } else {
        state
    }
}

/// Threads with a run of their own that works, as the embedded workbench blocks
/// archiving on: every run counts, a child's or a superseded root's too.
pub(super) fn working_threads(cache: &Cache) -> HashSet<&str> {
    cache
        .runs
        .values()
        .filter(|run| run_is_working(run.phase))
        .map(|run| run.thread.as_str())
        .collect()
}

/// The tree of `project`'s threads. `unread` holds the threads whose finished work
/// was not opened yet.
pub fn project_tree<'a>(
    cache: &'a Cache,
    model: &'a SidebarModel,
    project: &str,
    unread: &BTreeSet<String>,
) -> ProjectTree<'a> {
    let threads: HashMap<&'a str, &'a ThreadSummary> = cache
        .threads
        .values()
        .filter(|thread| thread.project.as_str() == project)
        .map(|thread| (thread.id.as_str(), thread))
        .collect();
    let detail = |id: &str| model.details.get(id);

    // Version groups: a rewind's root is found by walking its sources.
    let version_root = |id: &'a str| -> &'a str {
        let mut current = id;
        let mut seen = HashSet::from([id]);
        while let Some(detail) = detail(current)
            && detail.lineage == Some(LineageType::Rewind)
            && let Some(parent) = detail.parent_thread.as_ref()
            && let Some((&parent, _)) = threads.get_key_value(parent.as_str())
            && seen.insert(parent)
        {
            current = parent;
        }
        current
    };
    let roots: HashMap<&'a str, &'a str> =
        threads.keys().map(|&id| (id, version_root(id))).collect();
    // A rewind source is replaced by the rewind: it is not the group's visible version.
    let sources: HashSet<&str> = threads
        .keys()
        .filter_map(|&id| {
            let detail = detail(id)?;
            (detail.lineage == Some(LineageType::Rewind))
                .then(|| detail.parent_thread.as_ref().map(|p| p.as_str()))
                .flatten()
        })
        .collect();
    let mut visible: HashMap<&'a str, &'a str> = HashMap::new();
    let newest = |id: &str| (threads[id].updated_at, id.to_owned());
    for (&id, &root) in &roots {
        let candidate = !sources.contains(id);
        let entry = visible.entry(root).or_insert(id);
        let current = *entry;
        let current_candidate = !sources.contains(current);
        // A member no rewind points to wins; among those, the newest. A group whose
        // every member is a source (cyclic links) still shows its newest member.
        if (candidate, newest(id)) > (current_candidate, newest(current)) {
            *entry = id;
        }
    }
    let shown_version = |id: &'a str| visible[roots[id]];

    // A thread hangs under its version root's parent, shown as that group's version.
    let display_parent = |id: &'a str| -> Option<&'a str> {
        let root = roots[id];
        let parent = threads[root]
            .lead_thread
            .as_ref()
            .or_else(|| detail(root).and_then(|d| d.parent_thread.as_ref()))?
            .as_str();
        Some(match threads.get_key_value(parent) {
            Some((&parent, _)) => shown_version(parent),
            None => parent,
        })
    };
    let kind = |id: &'a str| -> Option<BranchKind> {
        let root = roots[id];
        if threads[root].lead_thread.is_some() {
            return Some(BranchKind::Lane);
        }
        match detail(root)?.lineage? {
            LineageType::Escalation => Some(BranchKind::Escalation),
            LineageType::Fork => Some(BranchKind::Fork),
            LineageType::Rewind => None,
        }
    };

    let questions: HashSet<&str> = cache
        .questions
        .values()
        .filter(|q| q.is_pending() && q.target == QuestionTarget::User)
        .map(|q| q.requester.thread.as_str())
        .collect();
    let approvals: HashSet<&str> = cache
        .approvals
        .values()
        .filter(|a| a.state == ApprovalState::Pending)
        .map(|a| a.thread.as_str())
        .collect();
    // Families with a working run, across every cached thread.
    let running_threads = working_threads(cache);
    let running_families: HashSet<&str> = running_threads
        .iter()
        .filter_map(|id| Some(detail(id)?.family.as_ref()?.as_str()))
        .collect();

    let row = |id: &'a str| -> ThreadRow<'a> {
        let summary = threads[id];
        let detail = detail(id);
        let parent = display_parent(id);
        let state = run_state(cache, model, id);
        let running = state == RunState::Running;
        let status = if state == RunState::Failed || summary.state == ThreadState::Failed {
            RowStatus::Failed
        } else if running {
            RowStatus::Running
        } else {
            RowStatus::Other
        };
        let family_running = match detail.and_then(|d| d.family.as_ref()) {
            Some(family) => running_families.contains(family.as_str()),
            None => running_threads.contains(id),
        };
        ThreadRow {
            summary,
            title: detail.map_or(summary.title.as_str(), |d| d.title.as_str()),
            display_root: parent.is_none(),
            kind: kind(id).filter(|_| parent.is_some()),
            status,
            running,
            own: Attention {
                question: questions.contains(id),
                approval: approvals.contains(id),
                unread: unread.contains(id),
                external: false,
            },
            pinned: detail.is_some_and(|d| d.pinned),
            archived: detail.is_some_and(|d| d.archived),
            family_running,
            detail_known: detail.is_some(),
            active_root: detail.and_then(|d| d.active_root.as_deref()),
        }
    };

    let (archived, main): (Vec<_>, Vec<_>) = threads
        .keys()
        .filter(|&&id| shown_version(id) == id)
        .map(|&id| row(id))
        .partition(|row| row.archived);
    ProjectTree {
        main: arrange(main, &display_parent, true),
        archived: arrange(archived, &display_parent, false),
    }
}

/// Sorts a section and nests it. Both sections run newest first by the latest
/// activity in each thread's displayed subtree; the main one keeps pinned roots
/// first, then families running anywhere in the subtree (a lane does not move).
fn arrange<'a>(
    mut rows: Vec<ThreadRow<'a>>,
    display_parent: &impl Fn(&'a str) -> Option<&'a str>,
    main: bool,
) -> Vec<(ThreadRow<'a>, usize)> {
    let shown: HashSet<&str> = rows.iter().map(ThreadRow::id).collect();
    let mut activity: HashMap<&str, u64> = rows
        .iter()
        .map(|row| (row.id(), row.summary.updated_at.get()))
        .collect();
    let mut running: HashSet<&str> = HashSet::new();
    for row in &rows {
        let at = row.summary.updated_at.get();
        if row.running {
            running.insert(row.id());
        }
        // Cyclic parent links stop at the first repeated ancestor.
        let mut seen = HashSet::from([row.id()]);
        let mut current = display_parent(row.id());
        while let Some(parent) = current.filter(|p| shown.contains(p) && seen.insert(p)) {
            let latest = activity.entry(parent).or_default();
            *latest = (*latest).max(at);
            if row.running {
                running.insert(parent);
            }
            current = display_parent(parent);
        }
    }
    rows.sort_by(|left, right| {
        let pinned = |row: &ThreadRow<'_>| main && row.pinned && row.display_root;
        let lifts = |row: &ThreadRow<'_>| {
            main && row.summary.lead_thread.is_none() && running.contains(row.id())
        };
        pinned(right)
            .cmp(&pinned(left))
            .then_with(|| lifts(right).cmp(&lifts(left)))
            .then_with(|| activity[right.id()].cmp(&activity[left.id()]))
            .then_with(|| left.title.cmp(right.title))
            .then_with(|| left.id().cmp(right.id()))
    });
    nest(&rows, |row| row.id(), |row| display_parent(row.id()))
        .into_iter()
        .map(|(index, depth)| (rows[index], depth))
        .collect()
}

/// The project's threads that are not archived; a thread whose detail is not read
/// yet counts as live. Hidden rewound versions count, as in the embedded workbench.
pub fn live_count(cache: &Cache, model: &SidebarModel, project: &str) -> usize {
    cache
        .threads
        .values()
        .filter(|thread| thread.project.as_str() == project && !model.archived(thread.id.as_str()))
        .count()
}

/// `projects` in the order `sort` asks for. The core lists projects by id, so
/// registration order is not known here and is kept as given.
pub fn ordered_projects<'a>(
    projects: &[&'a ProjectSummary],
    cache: &Cache,
    model: &SidebarModel,
    sort: ProjectSort,
) -> Vec<&'a ProjectSummary> {
    let mut ordered = projects.to_vec();
    let name = |project: &ProjectSummary| project.display_name.to_lowercase();
    match sort {
        ProjectSort::Registration => {}
        ProjectSort::Name => {
            ordered.sort_by_cached_key(|project| (name(project), project.id.as_str().to_owned()));
        }
        ProjectSort::RecentActivity => {
            let mut latest: HashMap<&str, u64> = HashMap::new();
            for thread in cache
                .threads
                .values()
                .filter(|thread| !model.archived(thread.id.as_str()))
            {
                let at = latest.entry(thread.project.as_str()).or_default();
                *at = (*at).max(thread.updated_at.get());
            }
            // A project without a live thread sorts after every other one.
            ordered.sort_by_cached_key(|project| {
                (
                    std::cmp::Reverse(latest.get(project.id.as_str()).copied()),
                    name(project),
                    project.id.as_str().to_owned(),
                )
            });
        }
    }
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;
    use workbench_protocol::{
        ids::{ProjectId, RunId, ThreadId},
        namespaces::workspace::ThreadDetail,
        views::{RunRole, RunSummary},
        wire::{Revision, UnixMillis},
    };

    fn summary(id: &str, project: &str, updated: u64) -> ThreadSummary {
        ThreadSummary {
            id: ThreadId::new(id).unwrap(),
            project: ProjectId::new(project).unwrap(),
            title: format!("summary {id}"),
            state: ThreadState::Idle,
            lead_thread: None,
            revision: Revision(1),
            updated_at: UnixMillis(updated),
        }
    }

    fn detail(id: &str) -> ThreadDetail {
        ThreadDetail {
            thread: ThreadId::new(id).unwrap(),
            project: ProjectId::new("p").unwrap(),
            title: id.into(),
            family: Some(ThreadId::new(id).unwrap()),
            parent_thread: None,
            lineage: None,
            root_run: None,
            active_root: None,
            model_profile: None,
            chat_role: None,
            extra_read_dirs: Vec::new(),
            archived: false,
            pinned: false,
            revision: Revision(1),
        }
    }

    fn child(id: &str, parent: &str, lineage: LineageType) -> ThreadDetail {
        ThreadDetail {
            parent_thread: Some(ThreadId::new(parent).unwrap()),
            lineage: Some(lineage),
            family: Some(ThreadId::new(parent).unwrap()),
            ..detail(id)
        }
    }

    fn run(id: &str, thread: &str, phase: RunPhase) -> RunSummary {
        RunSummary {
            id: RunId::new(id).unwrap(),
            thread: ThreadId::new(thread).unwrap(),
            parent_run: None,
            role: RunRole::Orchestrator,
            phase,
            revision: Revision(1),
        }
    }

    struct World {
        cache: Cache,
        model: SidebarModel,
    }

    impl World {
        fn new(threads: &[(&str, u64)]) -> Self {
            let mut cache = Cache::default();
            for &(id, updated) in threads {
                cache.threads.insert(id.into(), summary(id, "p", updated));
            }
            Self {
                cache,
                model: SidebarModel::default(),
            }
        }

        fn detail(&mut self, detail: ThreadDetail) -> &mut Self {
            self.model
                .details
                .insert(detail.thread.as_str().to_owned(), detail);
            self
        }

        fn tree(&self) -> ProjectTree<'_> {
            project_tree(&self.cache, &self.model, "p", &BTreeSet::new())
        }
    }

    fn shape<'a>(rows: &[(ThreadRow<'a>, usize)]) -> Vec<(&'a str, usize)> {
        rows.iter().map(|(row, depth)| (row.id(), *depth)).collect()
    }

    fn find<'a, 'r>(rows: &'r [(ThreadRow<'a>, usize)], id: &str) -> &'r ThreadRow<'a> {
        &rows.iter().find(|(row, _)| row.id() == id).unwrap().0
    }

    // Given: a lead with a lane, a fork and an escalation under it
    // When: the tree is built
    // Then: each hangs under the lead with its kind; the lead is a root without one
    #[test]
    fn lanes_nest_under_their_lead_and_forks_and_escalations_under_their_parent() {
        let mut world = World::new(&[("lead", 10), ("lane", 9), ("fork", 8), ("esc", 7)]);
        world.cache.threads.get_mut("lane").unwrap().lead_thread =
            Some(ThreadId::new("lead").unwrap());
        world
            .detail(detail("lead"))
            .detail(detail("lane"))
            .detail(child("fork", "lead", LineageType::Fork))
            .detail(child("esc", "lead", LineageType::Escalation));
        let tree = world.tree();
        assert_eq!(
            shape(&tree.main),
            [("lead", 0), ("lane", 1), ("fork", 1), ("esc", 1)]
        );
        let kinds: Vec<_> = tree.main.iter().map(|(row, _)| row.kind).collect();
        assert_eq!(
            kinds,
            [
                None,
                Some(BranchKind::Lane),
                Some(BranchKind::Fork),
                Some(BranchKind::Escalation)
            ]
        );
        assert!(tree.main[0].0.display_root && !tree.main[1].0.display_root);
    }

    // Given: a root rewound once, whose rewound-away version has a fork
    // When: the tree is built
    // Then: only the rewind is listed, at the root's place, and the fork hangs under it
    #[test]
    fn rewound_versions_are_hidden_and_children_follow_the_visible_version() {
        let mut world = World::new(&[("root", 5), ("rewind", 6), ("fork", 4)]);
        world
            .detail(detail("root"))
            .detail(child("rewind", "root", LineageType::Rewind))
            .detail(child("fork", "root", LineageType::Fork));
        let tree = world.tree();
        assert_eq!(shape(&tree.main), [("rewind", 0), ("fork", 1)]);
        let rewind = find(&tree.main, "rewind");
        assert!(rewind.display_root && rewind.kind.is_none());
        assert_eq!(find(&tree.main, "fork").kind, Some(BranchKind::Fork));
    }

    #[test]
    fn an_unknown_detail_renders_as_a_root_with_the_summary_title() {
        let world = World::new(&[("t", 1)]);
        let tree = world.tree();
        assert_eq!(shape(&tree.main), [("t", 0)]);
        let row = tree.main[0].0;
        assert_eq!(row.title, "summary t");
        assert!(row.display_root && !row.pinned && !row.archived && !row.detail_known);
    }

    // Given: an old pinned root, a running root, two idle roots and two archived ones
    // When: the tree is built
    // Then: the pinned root leads, the running one follows, then the newest; the archived
    //       ones form their own list, newest first
    #[test]
    fn main_sorts_pinned_roots_then_running_then_newest_and_archived_is_newest_first() {
        let mut world = World::new(&[
            ("pinned", 1),
            ("running", 2),
            ("new", 30),
            ("old", 20),
            ("gone-old", 40),
            ("gone-new", 50),
        ]);
        world.cache.threads.get_mut("running").unwrap().state = ThreadState::Running;
        world
            .cache
            .runs
            .insert("r".into(), run("r", "running", RunPhase::Running));
        let pinned = ThreadDetail {
            pinned: true,
            ..detail("pinned")
        };
        let archived = |id| ThreadDetail {
            archived: true,
            ..detail(id)
        };
        world
            .detail(pinned)
            .detail(archived("gone-old"))
            .detail(archived("gone-new"));
        let tree = world.tree();
        assert_eq!(
            shape(&tree.main),
            [("pinned", 0), ("running", 0), ("new", 0), ("old", 0)]
        );
        assert_eq!(shape(&tree.archived), [("gone-new", 0), ("gone-old", 0)]);
        assert_eq!(find(&tree.main, "running").status, RowStatus::Running);
    }

    #[test]
    fn failed_comes_from_the_root_run_phase() {
        let mut world = World::new(&[("t", 1), ("u", 1)]);
        world
            .cache
            .runs
            .insert("r".into(), run("r", "t", RunPhase::Failed));
        // A failed child run does not fail the thread.
        let mut child_run = run("c", "u", RunPhase::Failed);
        child_run.parent_run = Some(RunId::new("ru").unwrap());
        world.cache.runs.insert("c".into(), child_run);
        world
            .cache
            .runs
            .insert("ru".into(), run("ru", "u", RunPhase::Done));
        world
            .detail(ThreadDetail {
                root_run: Some(RunId::new("r").unwrap()),
                ..detail("t")
            })
            .detail(ThreadDetail {
                root_run: Some(RunId::new("ru").unwrap()),
                ..detail("u")
            });
        let tree = world.tree();
        assert_eq!(find(&tree.main, "t").status, RowStatus::Failed);
        assert_eq!(find(&tree.main, "u").status, RowStatus::Other);
    }

    #[test]
    fn a_running_family_member_disables_archive() {
        let mut world = World::new(&[("root", 2), ("fork", 1), ("other", 3)]);
        world
            .detail(detail("root"))
            .detail(child("fork", "root", LineageType::Fork))
            .detail(detail("other"));
        world
            .cache
            .runs
            .insert("r".into(), run("r", "fork", RunPhase::WaitingForApproval));
        let tree = world.tree();
        assert!(find(&tree.main, "root").family_running);
        assert!(!find(&tree.main, "other").family_running);
    }

    // Given: an old thread whose root run finished a turn and waits for the user (the
    //       core's summary still says Running) and a newer idle thread
    // When: the tree is built
    // Then: the waiting thread is idle as in the embedded workbench: no spinner, its
    //       family can be archived, and it is not lifted above the newer thread
    #[test]
    fn a_root_waiting_for_the_user_is_idle_and_archivable() {
        let mut world = World::new(&[("waiting", 1), ("newer", 2)]);
        world.cache.threads.get_mut("waiting").unwrap().state = ThreadState::Running;
        world
            .cache
            .runs
            .insert("r".into(), run("r", "waiting", RunPhase::WaitingForUser));
        world
            .detail(ThreadDetail {
                root_run: Some(RunId::new("r").unwrap()),
                ..detail("waiting")
            })
            .detail(detail("newer"));
        let tree = world.tree();
        assert_eq!(shape(&tree.main), [("newer", 0), ("waiting", 0)]);
        let waiting = find(&tree.main, "waiting");
        assert!(!waiting.running && !waiting.family_running);
        assert_eq!(waiting.status, RowStatus::Other);
    }

    // Given: a root waiting on an approval, and another whose root waits for the user
    //        while a subagent of it runs
    // When: the tree is built
    // Then: both run, as in the embedded workbench
    #[test]
    fn an_approval_or_a_working_subagent_keeps_a_thread_running() {
        let mut world = World::new(&[("approval", 1), ("lead", 1)]);
        world.cache.runs.insert(
            "a".into(),
            run("a", "approval", RunPhase::WaitingForApproval),
        );
        world
            .cache
            .runs
            .insert("l".into(), run("l", "lead", RunPhase::WaitingForUser));
        let mut subagent = run("s", "lead", RunPhase::Running);
        subagent.parent_run = Some(RunId::new("l").unwrap());
        world.cache.runs.insert("s".into(), subagent);
        world
            .detail(ThreadDetail {
                root_run: Some(RunId::new("a").unwrap()),
                ..detail("approval")
            })
            .detail(ThreadDetail {
                root_run: Some(RunId::new("l").unwrap()),
                ..detail("lead")
            });
        let tree = world.tree();
        for id in ["approval", "lead"] {
            let row = find(&tree.main, id);
            assert!(row.running && row.family_running, "{id}");
            assert_eq!(row.status, RowStatus::Running, "{id}");
        }
    }

    #[test]
    fn the_project_count_excludes_archived_threads() {
        let mut world = World::new(&[("live", 1), ("unknown", 1), ("gone", 1)]);
        world
            .cache
            .threads
            .insert("elsewhere".into(), summary("elsewhere", "q", 1));
        world.detail(detail("live")).detail(ThreadDetail {
            archived: true,
            ..detail("gone")
        });
        assert_eq!(live_count(&world.cache, &world.model, "p"), 2);
    }

    #[test]
    fn projects_sort_by_name_or_by_their_newest_live_thread() {
        let project = |id: &str, name: &str| ProjectSummary {
            id: ProjectId::new(id).unwrap(),
            display_name: name.into(),
            thread_count: 0,
            revision: Revision(1),
        };
        let (a, b, c) = (
            project("a", "zeta"),
            project("b", "Alpha"),
            project("c", "mid"),
        );
        let mut world = World::new(&[]);
        world
            .cache
            .threads
            .insert("t1".into(), summary("t1", "a", 5));
        world
            .cache
            .threads
            .insert("t2".into(), summary("t2", "b", 9));
        world.detail(ThreadDetail {
            archived: true,
            ..detail("t2")
        });
        let listed = [&a, &b, &c];
        let ids = |sort| -> Vec<String> {
            ordered_projects(&listed, &world.cache, &world.model, sort)
                .iter()
                .map(|p| p.id.as_str().to_owned())
                .collect()
        };
        assert_eq!(ids(ProjectSort::Name), ["b", "c", "a"]);
        // "b"'s only thread is archived: it has no live activity.
        assert_eq!(ids(ProjectSort::RecentActivity), ["a", "b", "c"]);
    }
}
