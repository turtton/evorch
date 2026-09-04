use std::collections::BTreeMap;

use egui_dock::{DockArea, TabViewer};
use workspace_ui::{Panel, PanelId, PanelKind, SidebarState};

use super::{ConversationFocus, WorkbenchState};
use crate::diff::DiffModel;
use crate::model::commands::{GoalFormModel, MergeApprovalModel};
use crate::model::tasks::{AgentRunSource, TasksModel};
use crate::model::telemetry::TelemetryOverlay;
use crate::model::terminal::TerminalBuffer;
use crate::model::transcript_registry::TranscriptRegistry;
use crate::panes::{
    agent::agent_pane, agent_transcript::agent_transcript_pane, agents::agents_pane,
    diff::diff_pane, goal::goal_pane, merge::merge_pane, sidebar::sidebar_pane, tasks::tasks_pane,
    terminal::terminal_pane,
};
use crate::pty::PtySession;

impl<S: AgentRunSource> WorkbenchState<S> {
    pub(super) fn render(&mut self, ui: &mut egui::Ui) {
        let mut viewer = WorkbenchTabViewer {
            transcripts: &self.transcripts,
            telemetry: &self.telemetry,
            tasks: &mut self.tasks,
            terminal: &mut self.terminal,
            terminal_input: &mut self.terminal_input,
            pty: &mut self.pty,
            panels: &self.panels,
            sidebar: &self.sidebar,
            focus: &self.focus,
            diff: &self.diff,
            goal_form: &self.goal_form,
            merge: &self.merge,
        };
        DockArea::new(&mut self.dock).show_inside(ui, &mut viewer);
    }
}

struct WorkbenchTabViewer<'a, S> {
    transcripts: &'a TranscriptRegistry,
    telemetry: &'a TelemetryOverlay,
    tasks: &'a mut TasksModel<S>,
    terminal: &'a mut TerminalBuffer,
    terminal_input: &'a mut String,
    pty: &'a mut Option<PtySession>,
    panels: &'a BTreeMap<PanelId, Panel>,
    sidebar: &'a SidebarState,
    focus: &'a ConversationFocus,
    diff: &'a DiffModel,
    goal_form: &'a GoalFormModel,
    merge: &'a MergeApprovalModel,
}

impl<S: AgentRunSource> TabViewer for WorkbenchTabViewer<'_, S> {
    type Tab = PanelId;

    fn id(&mut self, tab: &mut Self::Tab) -> egui::Id {
        egui::Id::new(tab.as_str())
    }

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        self.panels
            .get(tab)
            .map(|panel| panel.title.clone())
            .unwrap_or_else(|| tab.to_string())
            .into()
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        let Some(panel) = self.panels.get(tab) else {
            return;
        };
        match panel.kind {
            PanelKind::Agent => {
                let transcript = match self.focus {
                    ConversationFocus::Thread => self.transcripts.thread(),
                    ConversationFocus::Agent(run_id) => self
                        .transcripts
                        .run(run_id)
                        .unwrap_or_else(|| self.transcripts.thread()),
                };
                agent_pane(ui, transcript);
            }
            PanelKind::Sidebar => sidebar_pane(ui, self.sidebar),
            PanelKind::Agents => agents_pane(ui, self.tasks, self.telemetry),
            PanelKind::AgentTranscript => {
                let run_id = panel.target.as_deref().unwrap_or_default();
                agent_transcript_pane(ui, run_id, self.transcripts.run(run_id));
            }
            PanelKind::Diff => diff_pane(ui, self.diff),
            PanelKind::Goal => goal_pane(ui, self.goal_form),
            PanelKind::MergeApproval => merge_pane(ui, self.merge),
            PanelKind::Terminal => terminal_pane(ui, self.terminal, self.terminal_input, self.pty),
            PanelKind::Tasks => tasks_pane(ui, self.tasks),
        }
    }
}
