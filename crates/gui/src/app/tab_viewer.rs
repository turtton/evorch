use std::collections::BTreeMap;

use egui_dock::TabViewer;
use workspace_ui::{Panel, PanelId, PanelKind, SidebarState};

use super::ConversationFocus;
use super::attention::{AttentionInputs, PaneAttention, attention_for};
use crate::diff::{DiffMode, DiffModel};
use crate::model::commands::{GoalFormModel, LoopStatusView, MergeApprovalModel};
use crate::model::tasks::{AgentRunSource, TasksModel};
use crate::model::telemetry::TelemetryOverlay;
use crate::model::terminal::TerminalBuffer;
use crate::model::transcript_registry::TranscriptRegistry;
use crate::panes::{
    agent::{AgentIdentity, AgentPaneAction, ConversationContext, agent_pane},
    agent_transcript::agent_transcript_pane,
    agents::{AgentsAction, agents_pane},
    diff::diff_pane,
    goal::{GoalAction, goal_pane},
    merge::{MergeAction, merge_pane},
    sidebar::{SidebarAction, sidebar_pane},
    tasks::tasks_pane,
    terminal::terminal_pane,
};
use crate::pty::PtySession;

pub(super) struct WorkbenchTabViewer<'a, S> {
    pub(super) transcripts: &'a TranscriptRegistry,
    pub(super) telemetry: &'a TelemetryOverlay,
    pub(super) tasks: &'a mut TasksModel<S>,
    pub(super) terminal: &'a mut TerminalBuffer,
    pub(super) terminal_input: &'a mut String,
    pub(super) pty: &'a mut Option<PtySession>,
    pub(super) panels: &'a BTreeMap<PanelId, Panel>,
    pub(super) sidebar: &'a SidebarState,
    pub(super) phases: &'a BTreeMap<String, workspace_ui::ThreadRunPhase>,
    pub(super) sidebar_action: &'a mut Option<SidebarAction>,
    pub(super) agents_action: &'a mut Option<AgentsAction>,
    pub(super) focus: &'a ConversationFocus,
    pub(super) diff: &'a DiffModel,
    pub(super) diff_request: &'a mut Option<DiffMode>,
    pub(super) goal_form: &'a GoalFormModel,
    pub(super) goal_action: &'a mut Option<GoalAction>,
    pub(super) loop_status: &'a LoopStatusView,
    pub(super) merge: &'a MergeApprovalModel,
    pub(super) merge_action: &'a mut Option<MergeAction>,
    pub(super) focus_request: &'a mut Option<&'static str>,
    pub(super) dock_tab_style: &'a egui_dock::TabStyle,
}

impl<S: AgentRunSource> WorkbenchTabViewer<'_, S> {
    fn attention_for_tab(&self, tab: &PanelId) -> PaneAttention {
        let Some(panel) = self.panels.get(tab) else {
            return PaneAttention::None;
        };
        attention_for(
            panel.kind,
            panel.target.as_deref(),
            &AttentionInputs {
                merge: &self.merge.view,
                loop_status: self.loop_status,
                phases: self.phases,
                tasks_rows: self.tasks.rows(),
            },
        )
    }

    fn agent_tab_ui(&mut self, ui: &mut egui::Ui) {
        let (transcript, identity) = match self.focus {
            ConversationFocus::Thread => (self.transcripts.thread(), None),
            ConversationFocus::Agent(run_id) => {
                let transcript = self
                    .transcripts
                    .run(run_id)
                    .unwrap_or_else(|| self.transcripts.thread());
                let row = self
                    .tasks
                    .rows()
                    .iter()
                    .find(|row| row.run_id.to_string() == *run_id);
                (
                    transcript,
                    Some(AgentIdentity {
                        run_id,
                        name: row.map(|row| row.name.as_str()),
                        role: row.map(|row| row.role.as_str()),
                    }),
                )
            }
        };
        let active_thread = self
            .sidebar
            .active_thread
            .as_ref()
            .and_then(|id| self.sidebar.threads.iter().find(|thread| &thread.id == id));
        let ctx = ConversationContext {
            has_project: self.sidebar.selected_project.is_some(),
            active_thread_title: active_thread.map(|thread| thread.title.as_str()),
            phase: active_thread
                .and_then(|thread| thread.run_ids.last())
                .and_then(|run_id| self.phases.get(run_id))
                .copied(),
            next_thread_title: format!("thread-{}", self.sidebar.threads.len() + 1),
        };
        if let Some(action) = agent_pane(ui, transcript, identity, ctx) {
            match action {
                AgentPaneAction::Agents(a) => *self.agents_action = Some(a),
                AgentPaneAction::Sidebar(a) => *self.sidebar_action = Some(a),
                AgentPaneAction::FocusPanel(id) => *self.focus_request = Some(id),
            }
        }
    }
}

impl<S: AgentRunSource> TabViewer for WorkbenchTabViewer<'_, S> {
    type Tab = PanelId;

    fn id(&mut self, tab: &mut Self::Tab) -> egui::Id {
        egui::Id::new(tab.as_str())
    }

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        let title = self
            .panels
            .get(tab)
            .map(|panel| panel.title.clone())
            .unwrap_or_else(|| tab.to_string());
        // egui_dock paints tab titles without accesskit nodes, so the "● " prefix
        // never reaches label-based test queries; the tab title text is unchanged.
        match self.attention_for_tab(tab).color() {
            Some(color) => egui::RichText::new(format!("● {title}"))
                .color(color)
                .into(),
            None => title.into(),
        }
    }

    fn tab_style_override(
        &self,
        tab: &Self::Tab,
        _global_style: &egui_dock::TabStyle,
    ) -> Option<egui_dock::TabStyle> {
        self.attention_for_tab(tab)
            .color()
            .map(|color| crate::theme::dock::attention_tab_style(self.dock_tab_style, color))
    }

    fn is_closeable(&self, tab: &Self::Tab) -> bool {
        tab.as_str().starts_with("agent-run-")
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        let Some(panel) = self.panels.get(tab) else {
            return;
        };
        match panel.kind {
            PanelKind::Agent => self.agent_tab_ui(ui),
            PanelKind::Sidebar => {
                if let Some(action) = sidebar_pane(ui, self.sidebar, self.phases) {
                    *self.sidebar_action = Some(action);
                }
            }
            PanelKind::Agents => {
                if let Some(action) = agents_pane(ui, self.tasks, self.telemetry) {
                    *self.agents_action = Some(action);
                }
            }
            PanelKind::AgentTranscript => {
                let run_id = panel.target.as_deref().unwrap_or_default();
                agent_transcript_pane(ui, run_id, self.transcripts.run(run_id));
            }
            PanelKind::Diff => {
                if let Some(mode) = diff_pane(ui, self.diff) {
                    *self.diff_request = Some(mode);
                }
            }
            PanelKind::Goal => {
                *self.goal_action = goal_pane(
                    ui,
                    self.goal_form,
                    self.loop_status,
                    self.merge.view.blocked.as_deref(),
                    self.sidebar.active_thread.is_some(),
                );
            }
            PanelKind::MergeApproval => {
                *self.merge_action = merge_pane(ui, self.merge);
            }
            PanelKind::Terminal => terminal_pane(ui, self.terminal, self.terminal_input, self.pty),
            PanelKind::Tasks => tasks_pane(ui, self.tasks),
        }
    }
}
