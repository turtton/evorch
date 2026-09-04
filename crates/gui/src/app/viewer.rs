use std::collections::BTreeMap;

use egui_dock::{DockArea, TabViewer};
use workspace_ui::{Panel, PanelId, PanelKind, SidebarState};

use super::{ConversationFocus, WorkbenchState};
use crate::diff::{DiffMode, DiffModel};
use crate::model::commands::{GoalFormModel, MergeApprovalModel};
use crate::model::tasks::{AgentRunSource, TasksModel};
use crate::model::telemetry::TelemetryOverlay;
use crate::model::terminal::TerminalBuffer;
use crate::model::transcript_registry::TranscriptRegistry;
use crate::panes::{
    agent::{AgentIdentity, agent_pane},
    agent_transcript::agent_transcript_pane,
    agents::{AgentsAction, agents_pane},
    diff::diff_pane,
    goal::{GoalAction, GoalFormSync, goal_pane},
    merge::{MergeAction, merge_pane},
    sidebar::{SidebarAction, set_sidebar_error, sidebar_pane},
    tasks::tasks_pane,
    terminal::terminal_pane,
};
use crate::pty::PtySession;

impl<S: AgentRunSource> WorkbenchState<S> {
    pub(super) fn render(&mut self, ui: &mut egui::Ui) {
        self.panels.retain(|panel_id, _| {
            !panel_id.as_str().starts_with("agent-run-") || self.dock.find_tab(panel_id).is_some()
        });
        let ctx = ui.ctx().clone();
        let mut sidebar_action = None;
        let mut agents_action = None;
        let mut diff_request = None;
        let mut goal_action = None;
        let mut merge_action = None;
        {
            let mut viewer = WorkbenchTabViewer {
                transcripts: &self.transcripts,
                telemetry: &self.telemetry,
                tasks: &mut self.tasks,
                terminal: &mut self.terminal,
                terminal_input: &mut self.terminal_input,
                pty: &mut self.pty,
                panels: &self.panels,
                sidebar: &self.sidebar,
                phases: &self.phases,
                sidebar_action: &mut sidebar_action,
                agents_action: &mut agents_action,
                focus: &self.focus,
                diff: &self.diff,
                diff_request: &mut diff_request,
                goal_form: &self.goal_form,
                goal_action: &mut goal_action,
                merge: &self.merge,
                merge_action: &mut merge_action,
            };
            DockArea::new(&mut self.dock).show_inside(ui, &mut viewer);
        }
        if let Some(mode) = diff_request {
            self.request_diff(mode);
        }
        if let Some(action) = agents_action {
            match action {
                AgentsAction::DrillDown(run_id) => self.drill_down(&run_id),
                AgentsAction::ReturnToThread => self.return_to_thread(),
                AgentsAction::OpenPane(run_id) => self.open_agent_pane(&run_id),
                AgentsAction::OpenDefaultPanes => self.open_default_agent_panes(),
            }
        }
        if let Some(action) = sidebar_action {
            let result = match action {
                SidebarAction::SelectProject(project_id) => self.select_project(project_id),
                SidebarAction::AddProject(path) => self.add_project(path).map(|_| ()),
                SidebarAction::CreateThread(title) => self.create_thread(title).map(|_| ()),
                SidebarAction::SwitchThread(thread_id) => self.switch_thread(thread_id),
                SidebarAction::TogglePin(thread_id) => self.toggle_pin(thread_id),
                SidebarAction::TogglePause(thread_id) => self.toggle_pause(thread_id),
                SidebarAction::SetTrust { path, trust } => self.set_allowed_trust(path, trust),
            };
            set_sidebar_error(&ctx, result.err().map(|error| error.to_string()));
        }
        if let Some(action) = goal_action {
            match action {
                GoalAction::Submit => self.submit_goal(),
                GoalAction::SyncForm(GoalFormSync {
                    goal,
                    references,
                    constraints,
                }) => {
                    let form = self.goal_form_mut();
                    form.goal = goal;
                    form.references = references;
                    form.constraints = constraints;
                }
            }
        }
        if let Some(MergeAction::Decide(decision)) = merge_action {
            self.decide_merge(decision);
        }
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
    phases: &'a BTreeMap<String, workspace_ui::ThreadRunPhase>,
    sidebar_action: &'a mut Option<SidebarAction>,
    agents_action: &'a mut Option<AgentsAction>,
    focus: &'a ConversationFocus,
    diff: &'a DiffModel,
    diff_request: &'a mut Option<DiffMode>,
    goal_form: &'a GoalFormModel,
    goal_action: &'a mut Option<GoalAction>,
    merge: &'a MergeApprovalModel,
    merge_action: &'a mut Option<MergeAction>,
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

    fn is_closeable(&self, tab: &Self::Tab) -> bool {
        tab.as_str().starts_with("agent-run-")
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        let Some(panel) = self.panels.get(tab) else {
            return;
        };
        match panel.kind {
            PanelKind::Agent => {
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
                if let Some(action) = agent_pane(ui, transcript, identity) {
                    *self.agents_action = Some(action);
                }
            }
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
                *self.goal_action =
                    goal_pane(ui, self.goal_form, self.sidebar.active_thread.is_some());
            }
            PanelKind::MergeApproval => {
                *self.merge_action = merge_pane(ui, self.merge);
            }
            PanelKind::Terminal => terminal_pane(ui, self.terminal, self.terminal_input, self.pty),
            PanelKind::Tasks => tasks_pane(ui, self.tasks),
        }
    }
}
