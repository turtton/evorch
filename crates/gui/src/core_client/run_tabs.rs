//! The run tabs of the core client: one closeable transcript tab per run, opened from a
//! Subagents row, or by itself when a subagent starts. The dock holds only their ids, so
//! `CoreTab` stays `Copy`. As the embedded workbench does with its subagent panes, the
//! tabs of a thread leave the dock while another thread is selected and come back when it
//! is selected again, and a subagent's tab follows its run: a running subagent has a split
//! of its own below the Subagents tab, a finished one is parked as a tab of its leaf.
use std::collections::{BTreeMap, BTreeSet};

use egui_dock::{DockState, Node, NodeIndex, SurfaceIndex, TabIndex};
use workbench_protocol::ids::{RunId, ThreadId};
use workbench_protocol::views::RunPhase;

use super::features::FeatureId;
use super::shell::{self, CoreTab};
use super::subagent_panes::{Change, ChangeKind};
use crate::dock::subagent_layout;

/// How many run tabs a layout file restores.
pub const MAX_RESTORED: usize = 64;

/// How many subagents get a split of their own at once. The dock's tree doubles its
/// storage with each split nested below the last, so past this a running subagent opens
/// as a tab of the last split instead (the embedded workbench has no such bound).
pub const MAX_SPLIT_PANES: usize = 6;

const HOME: CoreTab = CoreTab::Feature(FeatureId::Execution);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunTabId(u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPane {
    pub thread: ThreadId,
    pub run: RunId,
    pub name: String,
    pub child: bool,
}

/// Where a run tab goes when its thread is selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Opened with Open pane: beside the Subagents tab.
    Manual,
    /// A running subagent: a split of its own below the Subagents tab.
    Split,
    /// A finished subagent, parked in the Subagents tab's leaf; the order it was parked in.
    Parked(u64),
    /// Read from the layout file: placed by its run's phase when its thread is selected.
    Restored,
}

#[derive(Debug, Clone)]
pub struct RunTab {
    pub thread: ThreadId,
    pub run: RunId,
    pub name: Option<String>,
    /// The model the core reported for the live run, as `profile/model`.
    pub model: Option<String>,
    pub child: bool,
    pub title: String,
    pub placement: Placement,
    /// Opened with Open pane (or brought back from the layout file, which keeps only
    /// those): the tabs the layout file keeps. A subagent's own tab is opened again
    /// from the projection, as the embedded workbench rebuilds its subagent panes.
    pub opened: bool,
}

#[derive(Default)]
pub struct RunTabs {
    next: u32,
    tabs: BTreeMap<RunTabId, RunTab>,
    /// The tabs taken out of the dock while another thread than theirs is selected.
    hidden: BTreeSet<RunTabId>,
    next_parked: u64,
}

fn title(run: &RunId, name: Option<&str>, child: bool) -> String {
    match (name, child) {
        (Some(name), true) => format!("Transcript: {name}"),
        (Some(name), false) => format!("{name} ({run})"),
        (None, _) => format!("Transcript ({run})"),
    }
}

impl RunTabs {
    /// The tab of `pane`'s run, reusing the one already open for it.
    pub fn open(&mut self, pane: RunPane) -> RunTabId {
        if let Some(id) = self.find(&pane.run) {
            // Its caller puts it in the dock again.
            self.hidden.remove(&id);
            if let Some(tab) = self.tabs.get_mut(&id) {
                tab.opened = true;
            }
            return id;
        }
        self.create(pane, Placement::Manual, true)
    }

    fn find(&self, run: &RunId) -> Option<RunTabId> {
        self.tabs
            .iter()
            .find(|(_, tab)| &tab.run == run)
            .map(|(id, _)| *id)
    }

    fn create(&mut self, pane: RunPane, placement: Placement, opened: bool) -> RunTabId {
        let name = (!pane.name.is_empty()).then_some(pane.name);
        let title = title(&pane.run, name.as_deref(), pane.child);
        self.next += 1;
        let id = RunTabId(self.next);
        self.tabs.insert(
            id,
            RunTab {
                thread: pane.thread,
                run: pane.run,
                name,
                model: None,
                child: pane.child,
                title,
                placement,
                opened,
            },
        );
        id
    }

    pub fn get(&self, id: RunTabId) -> Option<&RunTab> {
        self.tabs.get(&id)
    }

    /// Every tab, those out of the dock too.
    pub fn iter(&self) -> impl Iterator<Item = (RunTabId, &RunTab)> {
        self.tabs.iter().map(|(id, tab)| (*id, tab))
    }

    /// Names the run of `id` once the core said it, as the embedded workbench retitles
    /// a subagent's pane when its agent is known.
    pub fn set_name(&mut self, id: RunTabId, name: &str) {
        if let Some(tab) = self.tabs.get_mut(&id).filter(|_| !name.is_empty()) {
            tab.title = title(&tab.run, Some(name), tab.child);
            tab.name = Some(name.to_owned());
        }
    }

    /// Shows the model the core reported for the run of `id`.
    pub fn set_model(&mut self, id: RunTabId, model: &str) {
        if let Some(tab) = self
            .tabs
            .get_mut(&id)
            .filter(|tab| tab.model.as_deref() != Some(model))
        {
            tab.model = Some(model.to_owned());
        }
    }

    /// What the layout file keeps of the tabs opened with Open pane, those out of the
    /// dock too: the pane each was opened from, the most recently opened first, and at
    /// most [`MAX_RESTORED`], so the tabs opened last are the ones that come back. The
    /// tabs subagents opened by themselves are left out: the projection opens them again,
    /// and they never push an opened one out of the file.
    pub fn panes(&self) -> Vec<RunPane> {
        self.tabs
            .values()
            .rev()
            .filter(|tab| tab.opened)
            .take(MAX_RESTORED)
            .map(|tab| RunPane {
                thread: tab.thread.clone(),
                run: tab.run.clone(),
                name: tab.name.clone().unwrap_or_default(),
                child: tab.child,
            })
            .collect()
    }

    /// The tabs restored from the layout file, most recently opened first as
    /// [`Self::panes`] gives them: the first [`MAX_RESTORED`] are kept, in the order they
    /// were opened, so the next save keeps the newest again. Each is out of the dock until
    /// [`Self::sweep`] sees its thread selected and places it by its run's phase.
    pub fn restore_hidden(&mut self, panes: Vec<RunPane>) {
        let mut panes = panes;
        panes.truncate(MAX_RESTORED);
        for pane in panes.into_iter().rev() {
            let id = match self.find(&pane.run) {
                Some(id) => id,
                None => self.create(pane, Placement::Restored, true),
            };
            self.hidden.insert(id);
        }
    }

    /// Drops the tabs out of the dock whose thread `exists` no longer knows, as the
    /// restored tabs of a thread the core removed: no selection can bring them back.
    pub fn prune_hidden(&mut self, exists: impl Fn(&ThreadId) -> bool) {
        let hidden = &mut self.hidden;
        self.tabs.retain(|id, tab| {
            if !hidden.contains(id) || exists(&tab.thread) {
                return true;
            }
            hidden.remove(id);
            false
        });
    }

    /// The thread and run of each tab in the dock, whose history it shows.
    pub fn shown(&self) -> impl Iterator<Item = (&ThreadId, &RunId)> {
        self.shown_tabs().map(|(_, tab)| (&tab.thread, &tab.run))
    }

    /// The thread and run of each tab drawn: the front tab of a leaf that is not
    /// folded. Only these read their run's history, so the subagents parked behind the
    /// Subagents tab cost nothing until one is brought to the front.
    pub fn drawn<'s>(&'s self, dock: &DockState<CoreTab>) -> Vec<(&'s ThreadId, &'s RunId)> {
        dock.iter_leaves()
            .filter(|(_, leaf)| !leaf.collapsed)
            .filter_map(|(_, leaf)| match leaf.tabs.get(leaf.active.0) {
                Some(CoreTab::Run(id)) if !self.hidden.contains(id) => self.tabs.get(id),
                _ => None,
            })
            .map(|tab| (&tab.thread, &tab.run))
            .collect()
    }

    /// Each tab in the dock.
    pub fn shown_tabs(&self) -> impl Iterator<Item = (RunTabId, &RunTab)> {
        self.tabs
            .iter()
            .filter(|(id, _)| !self.hidden.contains(id))
            .map(|(id, tab)| (*id, tab))
    }

    /// Drops every tab, those out of the dock too, as a reset layout does; returns
    /// their runs.
    pub fn clear(&mut self) -> Vec<RunId> {
        self.hidden.clear();
        std::mem::take(&mut self.tabs)
            .into_values()
            .map(|tab| tab.run)
            .collect()
    }

    /// Follows a subagent's run as the embedded workbench follows its lifecycle events:
    /// a start opens its tab with a split of its own (or brings a parked one back to
    /// one), a resume brings a parked one back, and a finish parks it, opening it parked
    /// when it was closed. The tab of a thread not `selected` only changes where it will
    /// go. The focus stays where it was.
    pub fn apply(
        &mut self,
        dock: &mut DockState<CoreTab>,
        change: &Change,
        selected: Option<&ThreadId>,
    ) {
        let existing = self.find(&change.run);
        let parked = existing
            .and_then(|id| self.tabs.get(&id))
            .is_some_and(|tab| matches!(tab.placement, Placement::Parked(_)));
        match (change.kind, existing) {
            (ChangeKind::Start, None) => {
                let id = self.create_subagent(change, selected);
                if !self.hidden.contains(&id) {
                    self.dock_split(dock, id);
                }
            }
            (ChangeKind::Start | ChangeKind::Resume, Some(id)) if parked => self.unpark(dock, id),
            (ChangeKind::Start | ChangeKind::Resume, _) => {}
            (ChangeKind::Finish, existing) => {
                let id = existing.unwrap_or_else(|| self.create_subagent(change, selected));
                self.park(dock, id);
            }
        }
    }

    fn create_subagent(&mut self, change: &Change, selected: Option<&ThreadId>) -> RunTabId {
        let pane = RunPane {
            thread: change.thread.clone(),
            run: change.run.clone(),
            name: String::new(),
            child: true,
        };
        let id = self.create(pane, Placement::Split, false);
        if selected != Some(&change.thread) {
            self.hidden.insert(id);
        }
        id
    }

    fn placement(&self, tab: &CoreTab) -> Option<Placement> {
        match tab {
            CoreTab::Run(id) => self.tabs.get(id).map(|tab| tab.placement),
            _ => None,
        }
    }

    fn is_subagent(&self, tab: &CoreTab) -> bool {
        *tab == HOME
            || matches!(
                self.placement(tab),
                Some(Placement::Split | Placement::Parked(_))
            )
    }

    fn is_split(&self, tab: &CoreTab) -> bool {
        self.placement(tab) == Some(Placement::Split)
    }

    fn is_parked(&self, tab: &CoreTab) -> bool {
        matches!(self.placement(tab), Some(Placement::Parked(_)))
    }

    fn leaves(&self, dock: &DockState<CoreTab>) -> Vec<NodeIndex> {
        subagent_layout::subagent_leaves(dock.main_surface(), |tab| self.is_subagent(tab))
    }

    fn leaf_tabs(dock: &DockState<CoreTab>, node: NodeIndex) -> &[CoreTab] {
        dock.main_surface()[node].tabs().unwrap_or_default()
    }

    fn equalize(&self, dock: &mut DockState<CoreTab>) {
        let leaves = self.leaves(dock);
        subagent_layout::equalize_subagent_fractions(dock.main_surface_mut(), &leaves);
    }

    /// Give the tab of `id`, out of the dock, a split below the last subagent leaf, or a
    /// place behind the last split's tabs once [`MAX_SPLIT_PANES`] have one.
    fn dock_split(&mut self, dock: &mut DockState<CoreTab>, id: RunTabId) {
        let focus = subagent_layout::focus_snapshot(dock);
        let leaves = self.leaves(dock);
        let splits = leaves
            .iter()
            .filter(|node| {
                Self::leaf_tabs(dock, **node)
                    .iter()
                    .any(|t| self.is_split(t))
            })
            .count();
        let bottom = leaves.last().copied();
        match bottom {
            Some(node) if splits >= MAX_SPLIT_PANES => {
                if let Node::Leaf(leaf) = &mut dock.main_surface_mut()[node] {
                    leaf.tabs.push(CoreTab::Run(id));
                }
            }
            _ => subagent_layout::dock_below(dock.main_surface_mut(), bottom, CoreTab::Run(id)),
        }
        subagent_layout::restore_focus(dock, focus, |tab| self.is_parked(tab));
        self.equalize(dock);
    }

    fn unpark(&mut self, dock: &mut DockState<CoreTab>, id: RunTabId) {
        if let Some(tab) = self.tabs.get_mut(&id) {
            tab.placement = Placement::Split;
        }
        if self.hidden.contains(&id) {
            return;
        }
        if let Some(path) = dock.find_tab(&CoreTab::Run(id)) {
            dock.remove_tab(path);
        }
        self.dock_split(dock, id);
    }

    /// Park the tab of `id` after the ones parked before it, as the embedded workbench
    /// parks a finished subagent: every parked tab in the dock moves, in the order parked,
    /// into the Subagents tab's leaf (after the Conversation when it shares it), and the
    /// Subagents tab comes to the front of it.
    fn park(&mut self, dock: &mut DockState<CoreTab>, id: RunTabId) {
        if let Some(tab) = self
            .tabs
            .get_mut(&id)
            .filter(|tab| !matches!(tab.placement, Placement::Parked(_)))
        {
            tab.placement = Placement::Parked(self.next_parked);
            self.next_parked += 1;
        }
        if !self.hidden.contains(&id) {
            self.park_shown(dock);
        }
    }

    fn park_shown(&mut self, dock: &mut DockState<CoreTab>) {
        let focus = subagent_layout::focus_snapshot(dock);
        let mut parked: Vec<_> = self
            .shown_tabs()
            .filter_map(|(id, tab)| match tab.placement {
                Placement::Parked(order) => Some((order, CoreTab::Run(id))),
                _ => None,
            })
            .collect();
        parked.sort_by_key(|(order, _)| *order);
        let target = self.park_target(dock);
        for (_, tab) in &parked {
            if let Some(path) = dock.find_tab(tab) {
                dock.remove_tab(path);
            }
        }
        subagent_layout::splice_parked(
            dock,
            &target,
            parked.into_iter().map(|(_, tab)| tab).collect(),
            |tab| *tab == CoreTab::Conversation,
        );
        subagent_layout::restore_focus(dock, focus, |tab| self.is_parked(tab));
        self.equalize(dock);
    }

    /// The Subagents tab on the main surface; else a running subagent's tab; else the
    /// Subagents tab put back at the start of the first subagent leaf, or to the right of
    /// everything, as the embedded workbench brings back its subagents' home.
    fn park_target(&self, dock: &mut DockState<CoreTab>) -> CoreTab {
        if dock
            .find_tab(&HOME)
            .is_some_and(|path| path.surface == SurfaceIndex::main())
        {
            return HOME;
        }
        let leaves = self.leaves(dock);
        let running = leaves.iter().find_map(|node| {
            Self::leaf_tabs(dock, *node)
                .iter()
                .find(|tab| self.is_split(tab))
                .copied()
        });
        if let Some(tab) = running {
            return tab;
        }
        if let Some(path) = dock.find_tab(&HOME) {
            dock.remove_tab(path);
        }
        let leaves = self.leaves(dock);
        if let Some(node) = leaves.first()
            && let Node::Leaf(leaf) = &mut dock.main_surface_mut()[*node]
        {
            // Anchored before the parked tabs leave, so the leaf is not pruned.
            leaf.tabs.insert(0, HOME);
            leaf.active = TabIndex(0);
        } else {
            subagent_layout::dock_below(dock.main_surface_mut(), None, HOME);
        }
        HOME
    }

    /// Put the tab of `id`, of the selected thread and out of the dock, where its
    /// placement says; a restored tab is placed by `phase`, its run's phase.
    fn place(&mut self, dock: &mut DockState<CoreTab>, id: RunTabId, phase: Option<RunPhase>) {
        let Some(tab) = self.tabs.get_mut(&id) else {
            return;
        };
        if tab.placement == Placement::Restored {
            tab.placement = match phase {
                Some(RunPhase::Done | RunPhase::Failed | RunPhase::Cancelled) if tab.child => {
                    self.next_parked += 1;
                    Placement::Parked(self.next_parked - 1)
                }
                Some(_) if tab.child => Placement::Split,
                _ => Placement::Manual,
            };
        }
        match tab.placement {
            Placement::Split => self.dock_split(dock, id),
            Placement::Parked(_) => self.park_shown(dock),
            Placement::Manual | Placement::Restored => {
                shell::place_beside(dock, CoreTab::Run(id), HOME);
            }
        }
    }

    /// Keeps the dock in step with `selected`: the tabs of another thread leave the dock
    /// (and are kept), those of `selected` that left come back where their placement puts
    /// them without being brought forward (a restored one by its run's phase, which
    /// `phase_of` gives), and a tab the dock lost (closed, or a reset layout) is dropped.
    /// Returns the runs whose history no tab in the dock wants any more.
    pub fn sweep(
        &mut self,
        dock: &mut DockState<CoreTab>,
        selected: Option<&ThreadId>,
        phase_of: impl Fn(&RunId) -> Option<RunPhase>,
    ) -> Vec<RunId> {
        let mut gone = Vec::new();
        let mut back = Vec::new();
        let hidden = &mut self.hidden;
        self.tabs.retain(|id, tab| {
            let shown = selected == Some(&tab.thread);
            match (dock.find_tab(&CoreTab::Run(*id)), shown) {
                (Some(_), true) => true,
                (Some(path), false) => {
                    dock.remove_tab(path);
                    hidden.insert(*id);
                    gone.push(tab.run.clone());
                    true
                }
                (None, true) if hidden.remove(id) => {
                    back.push((*id, phase_of(&tab.run)));
                    true
                }
                (None, false) if hidden.contains(id) => true,
                (None, _) => {
                    gone.push(tab.run.clone());
                    false
                }
            }
        });
        for (id, phase) in back {
            self.place(dock, id, phase);
        }
        if !gone.is_empty() {
            self.equalize(dock);
        }
        gone
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(thread: &str, run: &str, name: &str, child: bool) -> RunPane {
        RunPane {
            thread: ThreadId::new(thread).unwrap(),
            run: RunId::new(run).unwrap(),
            name: name.into(),
            child,
        }
    }

    #[test]
    fn open_reuses_the_tab_of_a_run_and_titles_it_as_the_embedded_workbench() {
        let mut tabs = RunTabs::default();
        let child = tabs.open(pane("t1", "run-a", "tester", true));
        assert_eq!(tabs.open(pane("t1", "run-a", "tester", true)), child);
        assert_eq!(tabs.get(child).unwrap().title, "Transcript: tester");
        let root = tabs.open(pane("t1", "run-r", "lead", false));
        assert_ne!(root, child);
        assert_eq!(tabs.get(root).unwrap().title, "lead (run-r)");
        let unnamed = tabs.open(pane("t1", "run-u", "", true));
        assert_eq!(tabs.get(unnamed).unwrap().title, "Transcript (run-u)");
        assert_eq!(tabs.get(unnamed).unwrap().name, None);
    }

    // Given: run tabs of t1 and t2 in the dock, and one of t1 closed
    // When: the tabs are swept while t1 is selected, then while t2 is
    // Then: the closed tab is dropped and t2's tab leaves the dock but is kept; once
    //       t2 is selected, its tab is back and t1's left; each run that left is
    //       returned once, the closed one too
    #[test]
    fn sweep_drops_closed_tabs_and_keeps_other_threads_tabs_out_of_the_dock() {
        let mut tabs = RunTabs::default();
        let kept = tabs.open(pane("t1", "run-a", "a", true));
        let closed = tabs.open(pane("t1", "run-b", "b", true));
        let other = tabs.open(pane("t2", "run-c", "c", true));
        let mut dock = DockState::new(vec![
            CoreTab::Conversation,
            CoreTab::Run(kept),
            CoreTab::Run(other),
        ]);
        let (t1, t2) = (ThreadId::new("t1").unwrap(), ThreadId::new("t2").unwrap());
        let run = |id: &str| RunId::new(id).unwrap();
        let mut gone = tabs.sweep(&mut dock, Some(&t1), |_| None);
        gone.sort();
        assert_eq!(gone, [run("run-b"), run("run-c")]);
        assert!(dock.find_tab(&CoreTab::Run(kept)).is_some());
        assert!(dock.find_tab(&CoreTab::Run(other)).is_none());
        assert!(tabs.get(closed).is_none() && tabs.get(other).is_some());
        assert_eq!(tabs.shown().count(), 1);
        assert!(tabs.sweep(&mut dock, Some(&t1), |_| None).is_empty());

        assert_eq!(tabs.sweep(&mut dock, Some(&t2), |_| None), [run("run-a")]);
        assert!(dock.find_tab(&CoreTab::Run(kept)).is_none());
        assert!(dock.find_tab(&CoreTab::Run(other)).is_some());
        let shown: Vec<_> = tabs.shown().map(|(_, run)| run.clone()).collect();
        assert_eq!(shown, [run("run-c")]);
        // The tab in front stays in front.
        let path = dock.find_tab(&CoreTab::Conversation).unwrap();
        let leaf = dock.leaf(path.node_path()).unwrap();
        assert_eq!(leaf.tabs[leaf.active.0], CoreTab::Conversation);
    }

    // Given: run tabs of t1 and t2 restored from the layout file, into a dock without them
    // When: the tabs are swept with no thread selected, then with t1 selected
    // Then: neither is in the dock nor read at first; once t1 is selected its tab is back
    //       right after Execution and read, while t2's stays out and is kept
    #[test]
    fn restored_tabs_stay_hidden_until_their_thread_is_selected() {
        let mut tabs = RunTabs::default();
        tabs.restore_hidden(vec![
            pane("t1", "run-a", "a", true),
            pane("t2", "run-b", "b", false),
        ]);
        let mut dock = DockState::new(vec![
            CoreTab::Feature(FeatureId::Execution),
            CoreTab::Feature(FeatureId::Teams),
        ]);
        assert!(tabs.sweep(&mut dock, None, |_| None).is_empty());
        assert_eq!(tabs.shown().count(), 0);
        assert_eq!(dock.iter_all_tabs().count(), 2);

        let t1 = ThreadId::new("t1").unwrap();
        assert!(tabs.sweep(&mut dock, Some(&t1), |_| None).is_empty());
        let shown: Vec<_> = tabs.shown().map(|(_, run)| run.as_str()).collect();
        assert_eq!(shown, ["run-a"]);
        let leaf: Vec<_> = dock.iter_all_tabs().map(|(_, tab)| *tab).collect();
        let restored = leaf[1];
        assert!(matches!(restored, CoreTab::Run(_)));
        assert_eq!(
            [leaf[0], leaf[2]],
            [
                CoreTab::Feature(FeatureId::Execution),
                CoreTab::Feature(FeatureId::Teams)
            ]
        );
        let mut panes = tabs.panes();
        panes.sort_by(|a, b| a.run.cmp(&b.run));
        assert_eq!(
            panes,
            [
                pane("t1", "run-a", "a", true),
                pane("t2", "run-b", "b", false)
            ]
        );
    }

    // Given: more tabs in the layout file than are restored, newest first
    // When: they are restored, and the panes are taken for the file again
    // Then: only the newest MAX_RESTORED are kept, in the same order
    #[test]
    fn restoring_keeps_the_newest_up_to_the_cap() {
        let saved: Vec<_> = (0..MAX_RESTORED + 3)
            .map(|index| pane("t1", &format!("run-{index}"), "", true))
            .collect();
        let mut tabs = RunTabs::default();
        tabs.restore_hidden(saved.clone());
        assert_eq!(tabs.panes(), saved[..MAX_RESTORED]);
    }

    // Given: a full layout file's worth of tabs restored, then a tab opened in the session
    // When: the panes are taken for the file and restored from it, as a restart does
    // Then: the new tab comes first and is restored; the oldest restored one is dropped
    #[test]
    fn a_tab_opened_after_a_full_restore_survives_the_next_restart() {
        let saved: Vec<_> = (0..MAX_RESTORED)
            .map(|index| pane("t1", &format!("run-{index}"), "", true))
            .collect();
        let mut tabs = RunTabs::default();
        tabs.restore_hidden(saved.clone());
        let new = pane("t2", "run-new", "new", true);
        tabs.open(new.clone());

        let mut restarted = RunTabs::default();
        restarted.restore_hidden(tabs.panes());
        let panes = restarted.panes();
        assert_eq!(panes.len(), MAX_RESTORED);
        assert_eq!(panes[0], new);
        assert_eq!(panes[1..], saved[..MAX_RESTORED - 1]);
    }

    // Given: a tab opened with Open pane, then more subagents than the file keeps starting
    //        and finishing in t1 and in t2, which is not selected
    // When: the panes are taken for the file, then Open pane is pressed on one of them
    // Then: only the opened tab is kept; the subagent's tab opened with Open pane joins it
    #[test]
    fn the_file_keeps_the_opened_tabs_not_the_subagents_own() {
        let mut tabs = RunTabs::default();
        let mut dock = focused_dock();
        let t1 = ThreadId::new("t1").unwrap();
        let opened = pane("t1", "run-m", "manual", true);
        tabs.open(opened.clone());
        for index in 0..MAX_RESTORED + 2 {
            let (thread, run) = (["t1", "t2"][index % 2], format!("run-{index}"));
            for kind in [ChangeKind::Start, ChangeKind::Finish] {
                tabs.apply(&mut dock, &change(kind, thread, &run), Some(&t1));
            }
        }
        assert_eq!(tabs.panes(), std::slice::from_ref(&opened));

        let auto = pane("t2", "run-1", "", true);
        tabs.open(auto.clone());
        assert_eq!(tabs.panes(), [auto, opened]);
    }

    // Given: restored tabs of t1 and t2, and a tab of t3 opened in the session
    // When: the hidden tabs are pruned with only t1 and t3 known
    // Then: t2's tab is dropped; t1's stays restorable and t3's stays open
    #[test]
    fn prune_drops_hidden_tabs_of_unknown_threads() {
        let mut tabs = RunTabs::default();
        tabs.restore_hidden(vec![
            pane("t1", "run-a", "a", true),
            pane("t2", "run-b", "b", true),
        ]);
        let open = tabs.open(pane("t3", "run-c", "c", true));
        tabs.prune_hidden(|thread| thread.as_str() != "t2");
        let runs: Vec<_> = tabs.panes().into_iter().map(|pane| pane.run).collect();
        assert_eq!(
            runs,
            [RunId::new("run-c").unwrap(), RunId::new("run-a").unwrap()]
        );
        assert!(tabs.get(open).is_some());
        let mut dock = DockState::new(vec![CoreTab::Feature(FeatureId::Execution)]);
        let t1 = ThreadId::new("t1").unwrap();
        tabs.sweep(&mut dock, Some(&t1), |_| None);
        assert_eq!(tabs.shown().count(), 1);
    }

    // Given: a run tab of t1 in the dock and one of t2 out of it, t1 selected
    // When: the layout is reset, and t2 is selected afterwards
    // Then: both runs are returned and neither tab comes back into the fresh dock
    #[test]
    fn clear_drops_the_tabs_of_other_threads_too() {
        let mut tabs = RunTabs::default();
        let shown = tabs.open(pane("t1", "run-a", "a", true));
        let hidden = tabs.open(pane("t2", "run-b", "b", true));
        let mut dock = DockState::new(vec![CoreTab::Run(shown), CoreTab::Run(hidden)]);
        let (t1, t2) = (ThreadId::new("t1").unwrap(), ThreadId::new("t2").unwrap());
        tabs.sweep(&mut dock, Some(&t1), |_| None);

        let mut runs = tabs.clear();
        runs.sort();
        assert_eq!(
            runs,
            [RunId::new("run-a").unwrap(), RunId::new("run-b").unwrap()]
        );
        let mut dock = DockState::new(vec![CoreTab::Conversation]);
        assert!(tabs.sweep(&mut dock, Some(&t2), |_| None).is_empty());
        assert!(dock.find_tab(&CoreTab::Run(hidden)).is_none());
        assert!(tabs.get(shown).is_none() && tabs.get(hidden).is_none());
    }

    fn change(kind: ChangeKind, thread: &str, run: &str) -> Change {
        Change {
            kind,
            thread: ThreadId::new(thread).unwrap(),
            run: RunId::new(run).unwrap(),
        }
    }

    /// The core client's default dock with the Conversation focused.
    fn focused_dock() -> DockState<CoreTab> {
        let mut dock = shell::default_dock();
        shell::focus(&mut dock, CoreTab::Conversation);
        dock
    }

    fn leaf_of(dock: &DockState<CoreTab>, tab: CoreTab) -> Vec<CoreTab> {
        let path = dock.find_tab(&tab).expect("the tab is in the dock");
        dock.leaf(path.node_path()).unwrap().tabs.clone()
    }

    fn front(dock: &DockState<CoreTab>, tab: CoreTab) -> CoreTab {
        let path = dock.find_tab(&tab).expect("the tab is in the dock");
        let leaf = dock.leaf(path.node_path()).unwrap();
        leaf.tabs[leaf.active.0]
    }

    fn focused(dock: &DockState<CoreTab>) -> Vec<CoreTab> {
        let path = dock.focused_leaf().expect("a focused leaf");
        dock.leaf(path).unwrap().tabs.clone()
    }

    fn tab_of(tabs: &RunTabs, run: &str) -> CoreTab {
        CoreTab::Run(tabs.find(&RunId::new(run).unwrap()).expect("a tab"))
    }

    // Given: the default dock, the Conversation focused, t1 selected
    // When: two subagents of t1 start, and one of t2
    // Then: each of t1's gets a split below the last, sharing the column equally, the
    //       focus stays; t2's waits out of the dock
    #[test]
    fn a_start_splits_below_the_subagents_and_keeps_the_focus() {
        let mut tabs = RunTabs::default();
        let mut dock = focused_dock();
        let t1 = ThreadId::new("t1").unwrap();
        let conversation = focused(&dock);
        for (thread, run) in [("t1", "run-a"), ("t1", "run-b"), ("t2", "run-c")] {
            tabs.apply(
                &mut dock,
                &change(ChangeKind::Start, thread, run),
                Some(&t1),
            );
        }
        let (a, b) = (tab_of(&tabs, "run-a"), tab_of(&tabs, "run-b"));
        assert_eq!(leaf_of(&dock, a), [a]);
        assert_eq!(leaf_of(&dock, b), [b]);
        assert!(dock.find_tab(&tab_of(&tabs, "run-c")).is_none());
        assert_eq!(focused(&dock), conversation);
        assert_eq!(
            tabs.get(tabs.find(&RunId::new("run-a").unwrap()).unwrap())
                .unwrap()
                .title,
            "Transcript (run-a)"
        );
        let leaves = tabs.leaves(&dock);
        assert_eq!(leaves.len(), 3, "Subagents, then run-a, then run-b");
        let Node::Vertical(top) = &dock.main_surface()[leaves[0].parent().unwrap()] else {
            panic!("the Subagents leaf was split");
        };
        assert!((top.fraction - 1.0 / 3.0).abs() < f32::EPSILON);
    }

    // Given: a running subagent's split, and a tab of t1 opened with Open pane
    // When: both runs finish, then the first starts again
    // Then: each is parked, in the order they finished, at the end of the Subagents
    //       leaf, which comes to the front; the restarted one gets its split back
    #[test]
    fn a_finish_parks_behind_subagents_and_a_restart_unparks() {
        let mut tabs = RunTabs::default();
        let mut dock = focused_dock();
        let t1 = ThreadId::new("t1").unwrap();
        let home = CoreTab::Feature(FeatureId::Execution);
        tabs.apply(
            &mut dock,
            &change(ChangeKind::Start, "t1", "run-a"),
            Some(&t1),
        );
        let manual = CoreTab::Run(tabs.open(pane("t1", "run-m", "manual", true)));
        shell::focus_beside(&mut dock, manual, home);
        shell::focus(&mut dock, CoreTab::Conversation);
        let conversation = focused(&dock);

        tabs.apply(
            &mut dock,
            &change(ChangeKind::Finish, "t1", "run-m"),
            Some(&t1),
        );
        tabs.apply(
            &mut dock,
            &change(ChangeKind::Finish, "t1", "run-a"),
            Some(&t1),
        );
        let a = tab_of(&tabs, "run-a");
        let leaf = leaf_of(&dock, home);
        assert_eq!(leaf[leaf.len() - 2..], [manual, a]);
        assert_eq!(front(&dock, home), home);
        assert_eq!(tabs.leaves(&dock).len(), 1, "no split is left");
        assert_eq!(focused(&dock), conversation);

        tabs.apply(
            &mut dock,
            &change(ChangeKind::Start, "t1", "run-a"),
            Some(&t1),
        );
        assert_eq!(leaf_of(&dock, a), [a]);
        assert_eq!(focused(&dock), conversation);
    }

    // Given: t1 selected, one running subagent and two finished ones
    // When: the tabs to draw are asked for, then a parked one is brought to the front
    // Then: only the running one's split is drawn; the parked ones only once in front
    #[test]
    fn only_the_front_tab_of_a_leaf_is_drawn() {
        let mut tabs = RunTabs::default();
        let mut dock = focused_dock();
        let t1 = ThreadId::new("t1").unwrap();
        for (kind, run) in [
            (ChangeKind::Start, "run-a"),
            (ChangeKind::Finish, "run-b"),
            (ChangeKind::Finish, "run-c"),
        ] {
            tabs.apply(&mut dock, &change(kind, "t1", run), Some(&t1));
        }
        let drawn = |tabs: &RunTabs, dock: &DockState<CoreTab>| -> Vec<String> {
            tabs.drawn(dock)
                .into_iter()
                .map(|(_, run)| run.as_str().to_owned())
                .collect()
        };
        assert_eq!(tabs.shown().count(), 3);
        assert_eq!(drawn(&tabs, &dock), ["run-a"]);

        let c = dock.find_tab(&tab_of(&tabs, "run-c")).unwrap();
        dock.set_active_tab(c).unwrap();
        let mut now = drawn(&tabs, &dock);
        now.sort();
        assert_eq!(now, ["run-a", "run-c"]);
    }

    // Given: a subagent's tab the person closed, and a subagent of t2 never opened
    // When: both finish while t1 is selected, then t2 is selected
    // Then: t1's comes back parked; t2's is parked out of the dock and parked once shown
    #[test]
    fn a_closed_or_unseen_subagent_is_parked_when_it_finishes() {
        let mut tabs = RunTabs::default();
        let mut dock = focused_dock();
        let (t1, t2) = (ThreadId::new("t1").unwrap(), ThreadId::new("t2").unwrap());
        let home = CoreTab::Feature(FeatureId::Execution);
        tabs.apply(
            &mut dock,
            &change(ChangeKind::Start, "t1", "run-a"),
            Some(&t1),
        );
        let path = dock.find_tab(&tab_of(&tabs, "run-a")).unwrap();
        dock.remove_tab(path);
        assert_eq!(tabs.sweep(&mut dock, Some(&t1), |_| None).len(), 1);

        tabs.apply(
            &mut dock,
            &change(ChangeKind::Finish, "t1", "run-a"),
            Some(&t1),
        );
        tabs.apply(
            &mut dock,
            &change(ChangeKind::Finish, "t2", "run-b"),
            Some(&t1),
        );
        assert!(leaf_of(&dock, home).contains(&tab_of(&tabs, "run-a")));
        assert!(dock.find_tab(&tab_of(&tabs, "run-b")).is_none());

        tabs.sweep(&mut dock, Some(&t2), |_| None);
        let leaf = leaf_of(&dock, home);
        assert_eq!(leaf.last(), Some(&tab_of(&tabs, "run-b")));
        assert_eq!(front(&dock, home), home);
    }

    // Given: a running subagent's split and a parked one of t1
    // When: t2 is selected, then t1 again
    // Then: both leave the dock, and come back where they were: a split and a parked tab
    #[test]
    fn sweep_brings_subagents_back_by_placement() {
        let mut tabs = RunTabs::default();
        let mut dock = focused_dock();
        let (t1, t2) = (ThreadId::new("t1").unwrap(), ThreadId::new("t2").unwrap());
        let home = CoreTab::Feature(FeatureId::Execution);
        tabs.apply(
            &mut dock,
            &change(ChangeKind::Start, "t1", "run-a"),
            Some(&t1),
        );
        tabs.apply(
            &mut dock,
            &change(ChangeKind::Finish, "t1", "run-b"),
            Some(&t1),
        );
        tabs.sweep(&mut dock, Some(&t2), |_| None);
        assert_eq!(tabs.shown().count(), 0);
        assert_eq!(tabs.leaves(&dock).len(), 1);

        tabs.sweep(&mut dock, Some(&t1), |_| None);
        let (a, b) = (tab_of(&tabs, "run-a"), tab_of(&tabs, "run-b"));
        assert_eq!(leaf_of(&dock, a), [a]);
        assert!(leaf_of(&dock, home).contains(&b));
    }

    // Given: tabs restored from the layout file: a running child, a finished child, a
    //        child whose run is unknown and a root
    // When: their thread is selected
    // Then: the running child gets a split, the finished one is parked, the others go
    //       beside the Subagents tab
    #[test]
    fn restored_tabs_are_placed_by_their_runs_phase() {
        let mut tabs = RunTabs::default();
        tabs.restore_hidden(vec![
            pane("t1", "run-a", "a", true),
            pane("t1", "run-b", "b", true),
            pane("t1", "run-c", "c", true),
            pane("t1", "run-r", "r", false),
        ]);
        let mut dock = focused_dock();
        let t1 = ThreadId::new("t1").unwrap();
        let home = CoreTab::Feature(FeatureId::Execution);
        tabs.sweep(&mut dock, Some(&t1), |run| match run.as_str() {
            "run-a" | "run-r" => Some(RunPhase::Running),
            "run-b" => Some(RunPhase::Done),
            _ => None,
        });
        let a = tab_of(&tabs, "run-a");
        assert_eq!(leaf_of(&dock, a), [a]);
        let leaf = leaf_of(&dock, home);
        for run in ["run-b", "run-c", "run-r"] {
            assert!(leaf.contains(&tab_of(&tabs, run)), "{run} in {leaf:?}");
        }
    }

    // Given: more subagents start than get a split of their own
    // Then: past MAX_SPLIT_PANES, each opens as a tab of the last split, behind it
    #[test]
    fn splits_stop_at_the_bound_and_later_subagents_open_as_tabs() {
        let mut tabs = RunTabs::default();
        let mut dock = focused_dock();
        let t1 = ThreadId::new("t1").unwrap();
        let count = MAX_SPLIT_PANES + 2;
        for index in 0..count {
            let run = format!("run-{index}");
            tabs.apply(&mut dock, &change(ChangeKind::Start, "t1", &run), Some(&t1));
        }
        assert_eq!(tabs.leaves(&dock).len(), MAX_SPLIT_PANES + 1);
        let last = tab_of(&tabs, &format!("run-{}", MAX_SPLIT_PANES - 1));
        let leaf = leaf_of(&dock, last);
        assert_eq!(leaf.len(), 3);
        assert_eq!(front(&dock, last), last);
    }
}
