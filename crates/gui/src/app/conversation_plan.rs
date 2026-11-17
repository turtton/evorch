//! The ConversationPlan panel in the Workbench: opening it on request,
//! following the shown conversation, and refreshing on change notifications.
//!
//! The panel is never part of the default layout. It opens only from the
//! band's open button, beside the subagents, and a plan update never opens
//! it or brings a closed one back. A saved layout does not restore it either:
//! after a restart it opens again on request.

use std::sync::Arc;

use egui_dock::{NodeIndex, NodePath, SurfaceIndex, TabIndex};
use event_bus::{ConversationPlanEvent, Event, EventKind};
use workbench_protocol::ids::ThreadId;
use workbench_protocol::wire::Revision;
use workspace_ui::{Panel, PanelId, PanelKind};

use super::WorkbenchState;
use crate::model::conversation_plan::ConversationPlanModel;
use crate::model::tasks::AgentRunSource;
use crate::panes::conversation_plan::{PlanBand, PlanPanelAction, plan_panel};

pub(crate) const PANEL: &str = crate::panes::conversation_plan::PANEL_ID;

/// The plan model and whether its panel was drawn since the last poll.
#[derive(Default)]
pub struct ConversationPlanPane {
    model: ConversationPlanModel,
    rendered: bool,
    shown_last_frame: bool,
    waker_installed: bool,
}

impl ConversationPlanPane {
    pub(super) fn render(&mut self, ui: &mut egui::Ui) {
        if !self.shown_last_frame {
            // Panel reads start at the next poll; make sure there is one.
            ui.ctx().request_repaint();
        }
        self.rendered = true;
        match plan_panel(ui, self.model.state()) {
            Some(PlanPanelAction::Select(task)) => {
                self.model.state_mut().select(&task);
                ui.ctx().request_repaint();
            }
            Some(PlanPanelAction::HistoryPage(page)) => {
                self.model.state_mut().set_history_page(page);
                ui.ctx().request_repaint();
            }
            None => {}
        }
    }

    /// What the composer band shows, or `None` when no plan can be read.
    pub(crate) fn band(&self) -> Option<PlanBand<'_>> {
        let state = self.model.state();
        let thread = state.thread()?;
        self.model.is_wired().then(|| PlanBand {
            summary: state.summary().filter(|summary| &summary.thread == thread),
        })
    }
}

fn protocol_thread(id: &str) -> Option<ThreadId> {
    ThreadId::new(id).ok()
}

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Read conversation plans through `model`.
    pub fn with_conversation_plan(mut self, model: ConversationPlanModel) -> Self {
        self.conversation_plan.model = model;
        self.conversation_plan.waker_installed = false;
        self.sync_conversation_plan_thread();
        self
    }

    pub fn conversation_plan(&self) -> &ConversationPlanModel {
        &self.conversation_plan.model
    }

    /// Show the plan panel beside the subagents, or bring an open one forward.
    pub fn open_conversation_plan_tab(&mut self) {
        let id = PanelId::new(PANEL);
        self.panels.entry(id.clone()).or_insert_with(|| Panel {
            id: id.clone(),
            kind: PanelKind::ConversationPlan,
            title: PanelKind::ConversationPlan.default_title().into(),
            target: None,
        });
        if self.dock.find_tab(&id).is_none() {
            self.place_conversation_plan_tab();
        }
        self.focus_panel(PANEL);
    }

    /// Adds the tab to the subagent region (as the agent browser does), else
    /// to the right of the main area.
    fn place_conversation_plan_tab(&mut self) {
        let id = PanelId::new(PANEL);
        let focus = self.subagent_focus_snapshot();
        let region = self
            .dock
            .find_tab(&PanelId::new("subagents-home"))
            .filter(|path| path.surface == SurfaceIndex::main())
            .map(|path| path.node)
            .or_else(|| self.subagent_leaves().first().copied());
        let leaf = region.and_then(|node| {
            self.dock
                .leaf_mut(NodePath {
                    surface: SurfaceIndex::main(),
                    node,
                })
                .ok()
        });
        match leaf {
            Some(leaf) => {
                leaf.tabs.push(id);
                leaf.active = TabIndex(leaf.tabs.len() - 1);
            }
            None => {
                let tree = self.dock.main_surface_mut();
                if tree.is_empty() {
                    *tree = egui_dock::Tree::new(vec![id]);
                } else {
                    tree.split_right(NodeIndex::root(), 0.7, vec![id]);
                }
            }
        }
        self.restore_subagent_focus(focus);
    }

    /// The panel shows live data only: a saved layout's tab is dropped.
    pub(super) fn drop_saved_conversation_plan(&mut self) {
        while let Some(path) = self.dock.find_tab(&PanelId::new(PANEL)) {
            self.dock.remove_tab(path);
        }
        self.panels
            .retain(|_, panel| panel.kind != PanelKind::ConversationPlan);
    }

    fn sync_conversation_plan_thread(&mut self) {
        let thread = self
            .sidebar
            .active_thread
            .as_ref()
            .and_then(|id| protocol_thread(&id.to_string()));
        self.conversation_plan.model.state_mut().set_thread(thread);
    }

    /// Follows the shown conversation, folds finished reads and starts due
    /// ones, once per frame. Panel reads wait until the panel is on screen.
    pub(super) fn poll_conversation_plan(&mut self, ctx: &egui::Context) {
        let pane = &mut self.conversation_plan;
        if !pane.waker_installed {
            let ctx = ctx.clone();
            pane.model
                .set_waker(Arc::new(move || ctx.request_repaint()));
            pane.waker_installed = true;
        }
        self.sync_conversation_plan_thread();
        let pane = &mut self.conversation_plan;
        let visible = std::mem::take(&mut pane.rendered);
        pane.shown_last_frame = visible;
        if pane.model.poll(visible) {
            ctx.request_repaint();
        }
    }

    /// A plan change only names its revision; the views read it again.
    pub(super) fn fold_conversation_plan_event(&mut self, event: &Event) {
        if let EventKind::ConversationPlan(ConversationPlanEvent::Changed {
            thread_id,
            revision,
            ..
        }) = &event.kind
            && let Some(thread) = protocol_thread(thread_id)
        {
            self.conversation_plan
                .model
                .state_mut()
                .invalidate(&thread, Revision(*revision));
        }
    }
}

#[cfg(test)]
#[path = "conversation_plan_tests.rs"]
mod tests;
