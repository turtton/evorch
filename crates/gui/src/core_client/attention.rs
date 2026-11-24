//! Which dock tabs of the core client ask to be looked at, as the embedded workbench
//! flags its conversation and transcript tabs: a run whose phase changed to one a person
//! should read stays flagged until its tab is drawn with the window focused.
use std::collections::BTreeMap;

use workbench_protocol::ids::ThreadId;
use workbench_protocol::views::RunSummary;
use workspace_ui::ThreadRunPhase;

use super::cache::Cache;
use super::run_tabs::{RunTabId, RunTabs};
use super::shell::CoreTab;
use crate::panes::tab_attention::{
    PaneAttention,
    ack::{AttentionAck, DisplayRevision},
    run_phase, strongest,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Surface {
    Conversation,
    Run(RunTabId),
}

impl Surface {
    const fn of(tab: CoreTab) -> Option<Self> {
        match tab {
            CoreTab::Conversation => Some(Self::Conversation),
            CoreTab::Run(id) => Some(Self::Run(id)),
            _ => None,
        }
    }
}

type Key = (Surface, String);

#[derive(Default)]
pub struct TabAttention {
    acks: BTreeMap<Key, AttentionAck>,
}

pub struct Displayed(Vec<(Key, DisplayRevision)>);

impl TabAttention {
    /// Follows the current `root` of the selected `thread` in the Conversation tab and each
    /// run tab's run, the tabs out of the dock too, so a thread selected again keeps what
    /// was read. While the thread's root is not known (its detail is fetched again), what
    /// was read of the thread's conversation is kept.
    pub fn observe(
        &mut self,
        thread: Option<&ThreadId>,
        root: Option<&RunSummary>,
        cache: &Cache,
        run_tabs: &RunTabs,
    ) {
        let mut observed: BTreeMap<Key, ThreadRunPhase> = BTreeMap::new();
        if let Some(root) = root {
            observed.insert(
                (Surface::Conversation, root.id.as_str().to_owned()),
                run_phase(root.phase),
            );
        }
        for (id, tab) in run_tabs.iter() {
            if let Some(run) = cache.runs.get(tab.run.as_str()) {
                observed.insert(
                    (Surface::Run(id), tab.run.as_str().to_owned()),
                    run_phase(run.phase),
                );
            }
        }
        let unresolved = thread.filter(|_| root.is_none());
        self.acks.retain(|key, _| {
            observed.contains_key(key)
                || matches!(
                    (key, unresolved),
                    ((Surface::Conversation, run), Some(thread))
                        if cache.runs.get(run).is_some_and(|run| &run.thread == thread)
                )
        });
        for (key, phase) in observed {
            let ack = self.acks.entry(key).or_default();
            if ack.observe(phase).is_err() {
                // A fresh lifetime invalidates outstanding tokens if the sequence is exhausted.
                *ack = AttentionAck::default();
                let _ = ack.observe(phase);
            }
        }
    }

    fn acks_of(&self, tab: CoreTab) -> impl Iterator<Item = (&Key, &AttentionAck)> {
        let surface = Surface::of(tab);
        self.acks
            .iter()
            .filter(move |((of, _), _)| Some(*of) == surface)
    }

    pub fn of(&self, tab: CoreTab) -> PaneAttention {
        strongest(self.acks_of(tab).map(|(_, ack)| ack))
    }

    /// The phase a run tab follows and whether it is unread, for the badge on top of the
    /// tab as the embedded workbench draws it on its transcript tabs.
    pub fn run_phase(&self, id: RunTabId) -> Option<(ThreadRunPhase, bool)> {
        self.acks_of(CoreTab::Run(id))
            .next()
            .map(|(_, ack)| (ack.phase(), ack.is_unread()))
    }

    pub fn displayed(&self, tab: CoreTab) -> Displayed {
        Displayed(
            self.acks_of(tab)
                .map(|(key, ack)| (key.clone(), ack.revision()))
                .collect(),
        )
    }

    /// Marks what was `displayed` as read; true when that cleared a flag, so the tab is
    /// drawn again without it.
    pub fn acknowledge(&mut self, displayed: Displayed, focused: Option<bool>) -> bool {
        let mut cleared = false;
        for (key, revision) in displayed.0 {
            if let Some(ack) = self.acks.get_mut(&key) {
                let unread = ack.is_unread();
                cleared |= ack.acknowledge_surface(Some(&revision), focused) && unread;
            }
        }
        cleared
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_client::run_tabs::RunPane;
    use egui_dock::DockState;
    use workbench_protocol::ids::RunId;
    use workbench_protocol::views::{RunPhase, RunRole};
    use workbench_protocol::wire::Revision;

    fn run(id: &str, thread: &str, parent: Option<&str>, phase: RunPhase) -> RunSummary {
        RunSummary {
            id: RunId::new(id).unwrap(),
            thread: ThreadId::new(thread).unwrap(),
            parent_run: parent.map(|parent| RunId::new(parent).unwrap()),
            role: RunRole::Orchestrator,
            phase,
            revision: Revision(1),
        }
    }

    fn cache(runs: &[RunSummary]) -> Cache {
        let mut cache = Cache::default();
        for run in runs {
            cache.runs.insert(run.id.as_str().to_owned(), run.clone());
        }
        cache
    }

    fn read(attention: &mut TabAttention, tab: CoreTab) -> bool {
        let displayed = attention.displayed(tab);
        attention.acknowledge(displayed, Some(true))
    }

    #[test]
    fn a_new_root_is_flagged_again_after_the_old_one_was_read() {
        // Given: a finished root that was read
        let one = run("run-1", "thr-1", None, RunPhase::Done);
        let two = run("run-2", "thr-2", None, RunPhase::Done);
        let cache = cache(&[one.clone(), two.clone()]);
        let tabs = RunTabs::default();
        let mut attention = TabAttention::default();
        attention.observe(Some(&one.thread), Some(&one), &cache, &tabs);
        assert_eq!(attention.of(CoreTab::Conversation), PaneAttention::Info);
        assert!(read(&mut attention, CoreTab::Conversation));
        assert_eq!(attention.of(CoreTab::Conversation), PaneAttention::None);

        // When: the conversation follows another root
        attention.observe(Some(&two.thread), Some(&two), &cache, &tabs);

        // Then: its phase is unread
        assert_eq!(attention.of(CoreTab::Conversation), PaneAttention::Info);
        assert_eq!(attention.of(CoreTab::Diff), PaneAttention::None);
    }

    #[test]
    fn a_read_root_stays_read_while_the_thread_root_is_unknown() {
        // Given: thr-1 with a superseded and a current root, the current one read
        let old = run("run-0", "thr-1", None, RunPhase::Done);
        let current = run("run-1", "thr-1", None, RunPhase::Done);
        let other = run("run-2", "thr-2", None, RunPhase::Done);
        let cache = cache(&[old, current.clone(), other]);
        let tabs = RunTabs::default();
        let mut attention = TabAttention::default();
        attention.observe(Some(&current.thread), Some(&current), &cache, &tabs);
        assert!(read(&mut attention, CoreTab::Conversation));

        // When: the thread's root cannot be told for a while, then is known again
        attention.observe(Some(&current.thread), None, &cache, &tabs);
        assert_eq!(attention.of(CoreTab::Conversation), PaneAttention::None);
        attention.observe(Some(&current.thread), Some(&current), &cache, &tabs);

        // Then: it is still read; another thread's unknown root does not keep it
        assert_eq!(attention.of(CoreTab::Conversation), PaneAttention::None);
        attention.observe(Some(&ThreadId::new("thr-2").unwrap()), None, &cache, &tabs);
        attention.observe(Some(&current.thread), Some(&current), &cache, &tabs);
        assert_eq!(attention.of(CoreTab::Conversation), PaneAttention::Info);
    }

    #[test]
    fn a_hidden_run_tab_keeps_what_was_read() {
        // Given: a failed child run whose tab was read
        let mut cache = cache(&[run("run-1a", "thr-1", Some("run-1"), RunPhase::Failed)]);
        let mut tabs = RunTabs::default();
        let id = tabs.open(RunPane {
            thread: ThreadId::new("thr-1").unwrap(),
            run: RunId::new("run-1a").unwrap(),
            name: "child".to_owned(),
            child: true,
        });
        let mut attention = TabAttention::default();
        attention.observe(None, None, &cache, &tabs);
        assert_eq!(attention.of(CoreTab::Run(id)), PaneAttention::Error);
        assert!(read(&mut attention, CoreTab::Run(id)));

        // When: another thread is selected, so the tab leaves the dock
        let mut dock = DockState::new(vec![CoreTab::Run(id)]);
        tabs.sweep(&mut dock, Some(&ThreadId::new("thr-2").unwrap()), |_| None);
        assert_eq!(tabs.shown().count(), 0);
        attention.observe(None, None, &cache, &tabs);

        // Then: it stays read, and a later phase change flags it again
        assert_eq!(attention.of(CoreTab::Run(id)), PaneAttention::None);
        cache.runs.get_mut("run-1a").expect("run").phase = RunPhase::Done;
        attention.observe(None, None, &cache, &tabs);
        assert_eq!(attention.of(CoreTab::Run(id)), PaneAttention::Info);
    }

    #[test]
    fn acknowledging_needs_focus_and_the_drawn_revision() {
        let mut root = run("run-1", "thr-1", None, RunPhase::WaitingForUser);
        let mut cache = cache(&[root.clone()]);
        let tabs = RunTabs::default();
        let mut attention = TabAttention::default();
        attention.observe(Some(&root.thread), Some(&root), &cache, &tabs);

        let displayed = attention.displayed(CoreTab::Conversation);
        assert!(!attention.acknowledge(displayed, None));
        let displayed = attention.displayed(CoreTab::Conversation);
        assert!(!attention.acknowledge(displayed, Some(false)));

        let stale = attention.displayed(CoreTab::Conversation);
        root.phase = RunPhase::Failed;
        cache.runs.insert("run-1".to_owned(), root.clone());
        attention.observe(Some(&root.thread), Some(&root), &cache, &tabs);
        assert!(!attention.acknowledge(stale, Some(true)));
        assert_eq!(attention.of(CoreTab::Conversation), PaneAttention::Error);

        assert!(read(&mut attention, CoreTab::Conversation));
        assert!(!read(&mut attention, CoreTab::Conversation));
        assert_eq!(attention.of(CoreTab::Conversation), PaneAttention::None);
    }
}
