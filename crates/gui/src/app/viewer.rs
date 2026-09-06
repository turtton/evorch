use egui_dock::DockArea;

use super::WorkbenchState;
use super::tab_viewer::WorkbenchTabViewer;
use crate::model::tasks::AgentRunSource;
use crate::panes::{
    agents::AgentsAction,
    composer::ComposerAction,
    goal::{GoalAction, GoalFormSync},
    merge::MergeAction,
    provider_settings::{ProviderSettingsAction, provider_settings_modal},
    sidebar::{SidebarAction, set_sidebar_error},
};

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
        let mut composer_action = None;
        let mut merge_action = None;
        let mut focus_request = None;
        let dock_style = crate::theme::dock::dock_style(ui.style());
        let tab_style = dock_style.tab.clone();
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
                composer: &mut self.composer,
                provider_status: &self.provider_status,
                composer_action: &mut composer_action,
                loop_status: &self.loop_status,
                merge: &self.merge,
                merge_action: &mut merge_action,
                focus_request: &mut focus_request,
                dock_tab_style: &tab_style,
            };
            DockArea::new(&mut self.dock)
                .style(dock_style)
                .show_inside(ui, &mut viewer);
        }
        if let Some(id) = focus_request {
            self.focus_panel(id);
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
                GoalAction::OpenSettings => self.open_provider_settings(),
                GoalAction::PauseGoal => self.pause_goal(),
                GoalAction::ResumeGoal => self.resume_goal(),
                GoalAction::CancelGoal => self.cancel_goal(),
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
        if let Some(action) = composer_action {
            match action {
                ComposerAction::Send => self.submit_composer(),
                ComposerAction::OpenSettings => self.open_provider_settings(),
                ComposerAction::Complete(name) => {
                    self.composer_mut().input = format!("/{name} ");
                }
            }
        }
        if let Some(MergeAction::Decide(decision)) = merge_action {
            self.decide_merge(decision);
        }
        if self.provider_settings.open
            && let Some(action) = provider_settings_modal(ui.ctx(), &mut self.provider_settings)
        {
            match action {
                ProviderSettingsAction::Save => self.submit_provider_settings(),
                ProviderSettingsAction::Cancel => self.close_provider_settings(),
            }
        }
    }
}
