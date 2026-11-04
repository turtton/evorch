//! The Evaluations panel in the Workbench: opening it, polling its reads on
//! the frame loop, and learning from the event stream that an evaluation
//! finished.

use std::sync::Arc;
use std::time::Instant;

use event_bus::event::diagnostic_codes;
use event_bus::{Event, EventKind};
use workspace_ui::{Panel, PanelId, PanelKind};

use super::WorkbenchState;
use crate::model::evaluations::EvaluationsModel;
use crate::model::tasks::AgentRunSource;

const PANEL: &str = "evaluations-main";

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Read evaluations through `model`.
    pub fn with_evaluations(mut self, model: EvaluationsModel) -> Self {
        self.evaluations = model;
        self
    }

    pub fn evaluations(&self) -> &EvaluationsModel {
        &self.evaluations
    }

    pub fn evaluations_mut(&mut self) -> &mut EvaluationsModel {
        &mut self.evaluations
    }

    /// Show the Evaluations panel (next to the subagents, like the other
    /// side panels) and read evaluations again.
    pub fn open_evaluations_tab(&mut self) {
        let id = PanelId::new(PANEL);
        self.panels.entry(id.clone()).or_insert_with(|| Panel {
            id: id.clone(),
            kind: PanelKind::Evaluations,
            title: PanelKind::Evaluations.default_title().into(),
            target: None,
        });
        if self.dock.find_tab(&id).is_none() {
            let neighbor = self
                .dock
                .find_tab(&PanelId::new("subagents-home"))
                .or_else(|| self.dock.find_tab(&PanelId::new("diff-main")))
                .or_else(|| self.dock.find_tab(&PanelId::new("agent-main")));
            if let Some(path) = neighbor {
                self.dock.set_focused_node_and_surface(path.node_path());
            }
            self.dock.push_to_focused_leaf(id.clone());
        }
        self.evaluations.request_refresh();
        self.focus_panel(id.as_str());
    }

    /// Drain finished reads and start due ones, once per frame. Nothing is
    /// read while the panel is not on screen.
    pub(super) fn poll_evaluations(&mut self, ctx: &egui::Context) {
        if !self.evaluations_waker_installed {
            let ctx = ctx.clone();
            self.evaluations
                .set_waker(Arc::new(move || ctx.request_repaint()));
            self.evaluations_waker_installed = true;
        }
        let visible = self.evaluations.take_rendered();
        let poll = self.evaluations.poll_once(Instant::now(), visible);
        if poll.changed {
            ctx.request_repaint();
        }
        if let Some(delay) = poll.repaint_after {
            ctx.request_repaint_after(delay);
        }
    }

    /// An evaluation finishing makes what the panel holds stale. The event
    /// names no group, so everything is read again when the panel is next shown.
    pub(super) fn note_evaluation_event(&mut self, event: &Event) {
        if let EventKind::Diagnostic(diagnostic) = &event.kind
            && diagnostic.code == diagnostic_codes::EVALUATION_FINISHED
        {
            self.evaluations.request_refresh();
        }
    }
}
