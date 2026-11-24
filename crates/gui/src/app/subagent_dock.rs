use egui_dock::{Node, NodeIndex, TabIndex};
use event_bus::{AgentRunPhase, Event, EventKind, LifecycleEvent};
use workspace_ui::{Panel, PanelId, PanelKind};

use super::WorkbenchState;
use crate::dock::subagent_layout::{self, equalize_subagent_fractions};
use crate::model::tasks::AgentRunSource;

const REGION_HOME: &str = "subagents-home";

impl<S: AgentRunSource> WorkbenchState<S> {
    /// The display-only lifecycle projection is shared with other GUI windows;
    /// following their events must not execute runtime actions or approvals.
    pub(super) fn apply_run_display_event(&mut self, event: &Event) {
        match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStarted {
                run_id, agent_name, ..
            }) => {
                self.open_subagent_pane(run_id);
                self.unpark_subagent(run_id);
                if let Some(panel) = self
                    .panels
                    .get_mut(&PanelId::new(format!("agent-{run_id}")))
                    && !agent_name.trim().is_empty()
                {
                    panel.title = format!("Transcript: {agent_name}");
                }
            }
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) => {
                self.phases.insert(run_id.clone(), super::frame::phase(*to));
                match to {
                    AgentRunPhase::Stopped | AgentRunPhase::Done | AgentRunPhase::Error => {
                        let has_pane = self
                            .dock
                            .find_tab(&PanelId::new(format!("agent-{run_id}")))
                            .is_some();
                        let known_conversation = self.is_conversation_run(run_id);
                        if has_pane || !known_conversation {
                            self.park_completed_subagent(run_id);
                        }
                    }
                    AgentRunPhase::Pending | AgentRunPhase::Running => {
                        self.unpark_subagent(run_id);
                    }
                    AgentRunPhase::Waiting => {}
                }
            }
            _ => {}
        }
    }

    /// History rebuilds transcripts and the delegated-run index without live UI
    /// effects. Recreate their panes once ownership and final phases are known,
    /// even when no workspace layout was saved before the GUI closed.
    pub(super) fn restore_subagent_panes(&mut self) {
        let rows = self.tasks.rows().to_vec();
        for row in rows {
            let run_id = row.run_id.to_string();
            if self.thread_for_run(&run_id).is_none() || self.is_conversation_run(&run_id) {
                continue;
            }
            self.open_subagent_pane(&run_id);
            if !row.name.trim().is_empty()
                && let Some(panel) = self
                    .panels
                    .get_mut(&PanelId::new(format!("agent-{run_id}")))
            {
                panel.title = format!("Transcript: {}", row.name);
            }
            let phase = self
                .phases
                .get(&run_id)
                .copied()
                .unwrap_or_else(|| super::frame::phase(row.status));
            match phase {
                workspace_ui::ThreadRunPhase::Done
                | workspace_ui::ThreadRunPhase::Error
                | workspace_ui::ThreadRunPhase::Stopped => self.park_completed_subagent(&run_id),
                _ => self.unpark_subagent(&run_id),
            }
        }
        self.sync_subagent_thread_panes();
    }

    pub fn open_subagent_pane(&mut self, run_id: &str) {
        if self.is_conversation_run(run_id) {
            return;
        }
        let id = PanelId::new(format!("agent-{run_id}"));
        if self.panels.contains_key(&id) {
            return;
        }
        let focus = self.subagent_focus_snapshot();
        self.panels.insert(
            id.clone(),
            Panel {
                id: id.clone(),
                kind: PanelKind::SubagentTranscript,
                title: format!("Transcript ({run_id})"),
                target: Some(run_id.to_owned()),
            },
        );
        if !self.subagent_run_is_visible(run_id) {
            return;
        }
        self.dock_subagent_pane(id);
        self.restore_subagent_focus(focus);
        self.equalize_subagent_panes();
    }

    /// Give a subagent its own leaf below the last subagent leaf, or to the
    /// right of the workbench when there is none yet.
    fn dock_subagent_pane(&mut self, id: PanelId) {
        let bottom = self.subagent_leaves().last().copied();
        subagent_layout::dock_below(self.dock.main_surface_mut(), bottom, id);
    }

    /// A finished subagent that the runtime resumed leaves its parked tab and
    /// gets its own split pane again. Anything not parked is left alone.
    pub fn unpark_subagent(&mut self, run_id: &str) {
        let id = PanelId::new(format!("agent-{run_id}"));
        let Some(panel) = self.panels.get_mut(&id) else {
            return;
        };
        if !matches!(panel.kind, PanelKind::ParkedAgentTranscript(_)) {
            return;
        }
        panel.kind = PanelKind::SubagentTranscript;
        // Another thread's pane is not in the dock; switching to its thread
        // docks it as a running transcript.
        if !self.subagent_run_is_visible(run_id) {
            return;
        }
        let focus = self.subagent_focus_snapshot();
        if let Some(path) = self.dock.find_tab(&id) {
            self.dock.remove_tab(path);
        }
        self.dock_subagent_pane(id);
        self.restore_subagent_focus(focus);
        self.equalize_subagent_panes();
    }

    pub fn park_completed_subagent(&mut self, run_id: &str) {
        if self.is_conversation_run(run_id) {
            return;
        }
        let focus = self.subagent_focus_snapshot();
        let id = PanelId::new(format!("agent-{run_id}"));
        if !self.panels.contains_key(&id) {
            self.open_subagent_pane(run_id);
        }
        if !self.subagent_run_is_visible(run_id) {
            let order = self
                .panels
                .values()
                .filter_map(|panel| match panel.kind {
                    PanelKind::ParkedAgentTranscript(order) => Some(order),
                    _ => None,
                })
                .max()
                .map_or(0, |order| order + 1);
            if let Some(panel) = self.panels.get_mut(&id) {
                panel.kind = PanelKind::ParkedAgentTranscript(order);
            }
            return;
        }
        let mut parked: Vec<_> = self
            .panels
            .iter()
            .filter(|(_, panel)| {
                panel
                    .target
                    .as_deref()
                    .is_none_or(|run| self.subagent_run_is_visible(run))
            })
            .filter_map(|(id, panel)| match panel.kind {
                PanelKind::ParkedAgentTranscript(order) => Some((order, id.clone())),
                _ => None,
            })
            .collect();
        parked.sort();
        if !parked.iter().any(|(_, tab)| tab == &id) {
            let order = parked.last().map_or(0, |(order, _)| order + 1);
            if let Some(panel) = self.panels.get_mut(&id) {
                panel.kind = PanelKind::ParkedAgentTranscript(order);
            }
            parked.push((order, id));
        }
        // A retained, idle region stays at the top even when later runs start below it.
        let home = PanelId::new(REGION_HOME);
        let target = if self
            .dock
            .find_tab(&home)
            .is_some_and(|path| path.surface == egui_dock::SurfaceIndex::main())
        {
            home
        } else {
            let leaves = self.subagent_leaves();
            let running = leaves.iter().find_map(|node| {
                self.dock.main_surface()[*node]
                    .tabs()?
                    .iter()
                    .find(|id| {
                        self.panels
                            .get(*id)
                            .is_some_and(|p| p.kind == PanelKind::SubagentTranscript)
                    })
                    .cloned()
            });
            match running {
                Some(id) => id,
                None => {
                    self.panels.insert(
                        home.clone(),
                        Panel {
                            id: home.clone(),
                            kind: PanelKind::SubagentRegion,
                            title: "Subagents".into(),
                            target: None,
                        },
                    );
                    if let Some(path) = self.dock.find_tab(&home) {
                        self.dock.remove_tab(path);
                    }
                    let leaves = self.subagent_leaves();
                    if let Some(node) = leaves.first()
                        && let Node::Leaf(leaf) = &mut self.dock.main_surface_mut()[*node]
                    {
                        // Anchor before removals so the last region leaf cannot be pruned.
                        leaf.tabs.insert(0, home.clone());
                        leaf.active = TabIndex(0);
                    } else {
                        let tree = self.dock.main_surface_mut();
                        if tree.is_empty() {
                            *tree = egui_dock::Tree::new(vec![home.clone()]);
                        } else {
                            tree.split_right(NodeIndex::root(), 0.7, vec![home.clone()]);
                        }
                    }
                    home
                }
            }
        };
        // Only explicitly completed tabs move. Each removal can reindex the entire tree.
        for (_, tab) in &parked {
            if let Some(path) = self.dock.find_tab(tab) {
                self.dock.remove_tab(path);
            }
        }
        subagent_layout::splice_parked(
            &mut self.dock,
            &target,
            parked.into_iter().map(|(_, id)| id).collect(),
            |id| id.as_str() == "agent-main",
        );
        self.restore_subagent_focus(focus);
        self.equalize_subagent_panes();
    }

    pub(super) fn subagent_run_is_visible(&self, run_id: &str) -> bool {
        self.sidebar
            .threads
            .iter()
            .find(|thread| thread.run_ids.iter().any(|run| run == run_id))
            .is_some_and(|owner| Some(&owner.id) == self.sidebar.active_thread.as_ref())
    }

    /// Keep each thread's registered transcript panes available without showing
    /// another thread's runs in the current right-hand dock.
    pub(super) fn sync_subagent_thread_panes(&mut self) {
        let panes: Vec<_> = self
            .panels
            .values()
            .filter(|panel| {
                matches!(
                    panel.kind,
                    PanelKind::SubagentTranscript | PanelKind::ParkedAgentTranscript(_)
                )
            })
            .cloned()
            .collect();
        for panel in &panes {
            if panel
                .target
                .as_deref()
                .is_some_and(|run| !self.subagent_run_is_visible(run))
                && let Some(path) = self.dock.find_tab(&panel.id)
            {
                self.dock.remove_tab(path);
            }
        }
        for panel in panes {
            let Some(run_id) = panel.target.as_deref() else {
                continue;
            };
            if !self.subagent_run_is_visible(run_id) || self.dock.find_tab(&panel.id).is_some() {
                continue;
            }
            self.panels.remove(&panel.id);
            self.open_subagent_pane(run_id);
            let parked = matches!(panel.kind, PanelKind::ParkedAgentTranscript(_));
            if let Some(restored) = self.panels.get_mut(&panel.id) {
                restored.title.clone_from(&panel.title);
                if parked {
                    restored.kind = panel.kind;
                }
            }
            if parked {
                self.park_completed_subagent(run_id);
            }
        }
        self.equalize_subagent_panes();
    }

    pub(super) fn equalize_subagent_panes(&mut self) {
        let leaves = self.subagent_leaves();
        equalize_subagent_fractions(self.dock.main_surface_mut(), &leaves);
    }

    pub(super) fn subagent_leaves(&self) -> Vec<NodeIndex> {
        subagent_layout::subagent_leaves(self.dock.main_surface(), |id| {
            id.as_str() == REGION_HOME
                || self.panels.get(id).is_some_and(|panel| {
                    matches!(
                        panel.kind,
                        PanelKind::SubagentTranscript | PanelKind::ParkedAgentTranscript(_)
                    )
                })
        })
    }

    pub(super) fn subagent_focus_snapshot(&self) -> Option<Vec<PanelId>> {
        subagent_layout::focus_snapshot(&self.dock)
    }

    pub(super) fn restore_subagent_focus(&mut self, focus: Option<Vec<PanelId>>) {
        // Resolve saved tab identities after reindexing; a vanished leaf stays
        // unfocused rather than transferring focus to a new or parked pane.
        let panels = &self.panels;
        subagent_layout::restore_focus(&mut self.dock, focus, |id| {
            panels
                .get(id)
                .is_some_and(|p| matches!(p.kind, PanelKind::ParkedAgentTranscript(_)))
        });
    }
}

#[cfg(test)]
#[path = "subagent_dock_tests.rs"]
mod tests;
