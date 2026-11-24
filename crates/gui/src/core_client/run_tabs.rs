//! The run tabs of the core client: one closeable transcript tab per run, opened from an
//! Execution row. The dock holds only their ids, so `CoreTab` stays `Copy`. As the
//! embedded workbench does with its subagent panes, the tabs of a thread leave the dock
//! while another thread is selected and come back when it is selected again.
use std::collections::{BTreeMap, BTreeSet};

use egui_dock::DockState;
use workbench_protocol::ids::{RunId, ThreadId};

use super::features::FeatureId;
use super::shell::{self, CoreTab};

/// How many run tabs a layout file restores.
pub const MAX_RESTORED: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunTabId(u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPane {
    pub thread: ThreadId,
    pub run: RunId,
    pub name: String,
    pub child: bool,
}

#[derive(Debug, Clone)]
pub struct RunTab {
    pub thread: ThreadId,
    pub run: RunId,
    pub name: Option<String>,
    pub child: bool,
    pub title: String,
}

#[derive(Default)]
pub struct RunTabs {
    next: u32,
    tabs: BTreeMap<RunTabId, RunTab>,
    /// The tabs taken out of the dock while another thread than theirs is selected.
    hidden: BTreeSet<RunTabId>,
}

impl RunTabs {
    /// The tab of `pane`'s run, reusing the one already open for it.
    pub fn open(&mut self, pane: RunPane) -> RunTabId {
        if let Some((id, _)) = self.tabs.iter().find(|(_, tab)| tab.run == pane.run) {
            // Its caller puts it in the dock again.
            self.hidden.remove(id);
            return *id;
        }
        let child = pane.child;
        let name = (!pane.name.is_empty()).then_some(pane.name);
        let title = match (&name, pane.child) {
            (Some(name), true) => format!("Transcript: {name}"),
            (Some(name), false) => format!("{name} ({})", pane.run),
            (None, _) => format!("Transcript ({})", pane.run),
        };
        self.next += 1;
        let id = RunTabId(self.next);
        self.tabs.insert(
            id,
            RunTab {
                thread: pane.thread,
                run: pane.run,
                name,
                child,
                title,
            },
        );
        id
    }

    pub fn get(&self, id: RunTabId) -> Option<&RunTab> {
        self.tabs.get(&id)
    }

    /// What the layout file keeps of the tabs, those out of the dock too: the pane each
    /// was opened from, the most recently opened first, and at most [`MAX_RESTORED`], so
    /// the tabs opened last are the ones that come back.
    pub fn panes(&self) -> Vec<RunPane> {
        self.tabs
            .values()
            .rev()
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
    /// [`Self::sweep`] sees its thread selected and puts it beside Execution.
    pub fn restore_hidden(&mut self, panes: Vec<RunPane>) {
        let mut panes = panes;
        panes.truncate(MAX_RESTORED);
        for pane in panes.into_iter().rev() {
            let id = self.open(pane);
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
        self.tabs
            .iter()
            .filter(|(id, _)| !self.hidden.contains(id))
            .map(|(_, tab)| (&tab.thread, &tab.run))
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

    /// Keeps the dock in step with `selected`: the tabs of another thread leave the dock
    /// (and are kept), those of `selected` that left come back beside Execution without
    /// being brought forward, and a tab the dock lost (closed, or a reset layout) is
    /// dropped. Returns the runs whose history no tab in the dock wants any more.
    pub fn sweep(
        &mut self,
        dock: &mut DockState<CoreTab>,
        selected: Option<&ThreadId>,
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
                    back.push(*id);
                    true
                }
                (None, false) if hidden.contains(id) => true,
                (None, _) => {
                    gone.push(tab.run.clone());
                    false
                }
            }
        });
        for id in back {
            shell::place_beside(
                dock,
                CoreTab::Run(id),
                CoreTab::Feature(FeatureId::Execution),
            );
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
        let mut gone = tabs.sweep(&mut dock, Some(&t1));
        gone.sort();
        assert_eq!(gone, [run("run-b"), run("run-c")]);
        assert!(dock.find_tab(&CoreTab::Run(kept)).is_some());
        assert!(dock.find_tab(&CoreTab::Run(other)).is_none());
        assert!(tabs.get(closed).is_none() && tabs.get(other).is_some());
        assert_eq!(tabs.shown().count(), 1);
        assert!(tabs.sweep(&mut dock, Some(&t1)).is_empty());

        assert_eq!(tabs.sweep(&mut dock, Some(&t2)), [run("run-a")]);
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
        assert!(tabs.sweep(&mut dock, None).is_empty());
        assert_eq!(tabs.shown().count(), 0);
        assert_eq!(dock.iter_all_tabs().count(), 2);

        let t1 = ThreadId::new("t1").unwrap();
        assert!(tabs.sweep(&mut dock, Some(&t1)).is_empty());
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
        tabs.sweep(&mut dock, Some(&t1));
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
        tabs.sweep(&mut dock, Some(&t1));

        let mut runs = tabs.clear();
        runs.sort();
        assert_eq!(
            runs,
            [RunId::new("run-a").unwrap(), RunId::new("run-b").unwrap()]
        );
        let mut dock = DockState::new(vec![CoreTab::Conversation]);
        assert!(tabs.sweep(&mut dock, Some(&t2)).is_empty());
        assert!(dock.find_tab(&CoreTab::Run(hidden)).is_none());
        assert!(tabs.get(shown).is_none() && tabs.get(hidden).is_none());
    }
}
