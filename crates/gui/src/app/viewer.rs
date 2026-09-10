use egui_dock::DockArea;

use super::WorkbenchState;
use super::tab_viewer::WorkbenchTabViewer;
use crate::model::tasks::AgentRunSource;
use crate::panes::{
    agents::AgentsAction,
    composer::ComposerAction,
    provider_settings::{ProviderSettingsAction, provider_settings_modal},
    sidebar::{SidebarAction, set_sidebar_error},
};

impl<S: AgentRunSource> WorkbenchState<S> {
    pub(super) fn render(&mut self, ui: &mut egui::Ui) {
        self.ownership_ui(ui);
        self.panels.retain(|panel_id, _| {
            !panel_id.as_str().starts_with("agent-run-") || self.dock.find_tab(panel_id).is_some()
        });
        let ctx = ui.ctx().clone();
        let mut sidebar_action = None;
        let mut agents_action = None;
        let mut diff_request = None;
        let mut composer_action = None;
        let mut focus_request = None;
        let mut preference_action = None;
        let profiles = self.available_profiles();
        let dock_style = crate::theme::dock::dock_style(ui.style());
        let tab_style = dock_style.tab.clone();
        {
            let mut viewer = WorkbenchTabViewer {
                memory: &mut self.memory,
                arena: &mut self.arena,
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
                composer: &mut self.composer,
                provider_status: &self.provider_status,
                composer_action: &mut composer_action,
                focus_request: &mut focus_request,
                dock_tab_style: &tab_style,
                profiles: &profiles,
                picker_state: &mut self.model_picker,
                preference_action: &mut preference_action,
            };
            DockArea::new(&mut self.dock)
                .style(dock_style)
                .show_inside(ui, &mut viewer);
        }
        if let Some(id) = focus_request {
            self.focus_panel(id);
        }
        if let Some(preference) = preference_action {
            self.set_thread_model_preference(preference);
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
                SidebarAction::BrowseForProject => {
                    set_sidebar_error(&ctx, self.folder_picker.start().err());
                    ctx.request_repaint();
                    return;
                }
                SidebarAction::SelectProject(project_id) => self.select_project(project_id),
                SidebarAction::AddProject(path) => self.add_project(path).map(|_| ()),
                SidebarAction::CreateThread(title) => self.create_thread(title).map(|_| ()),
                SidebarAction::ForkThread(thread_id) => self.fork_thread(thread_id).map(|_| ()),
                SidebarAction::SwitchThread(thread_id) => self.switch_thread(thread_id),
                SidebarAction::TogglePin(thread_id) => self.toggle_pin(thread_id),
                SidebarAction::TogglePause(thread_id) => self.toggle_pause(thread_id),
                SidebarAction::SetTrust { path, trust } => self.set_allowed_trust(path, trust),
            };
            set_sidebar_error(&ctx, result.err().map(|error| error.to_string()));
        }
        if let Some(action) = composer_action {
            match action {
                ComposerAction::Send => self.submit_composer(),
                ComposerAction::Cancel => self.cancel_chat(),
                ComposerAction::OpenSettings => self.open_provider_settings(),
                ComposerAction::Complete(name) => {
                    self.composer_mut().input = format!("/{name} ");
                }
            }
        }
        if self.provider_settings.open
            && let Some(action) =
                provider_settings_modal(ui.ctx(), &mut self.provider_settings, &self.codex_auth)
        {
            match action {
                ProviderSettingsAction::Save => self.submit_provider_settings(),
                ProviderSettingsAction::Delete(name) => self.delete_provider_settings(name),
                ProviderSettingsAction::Cancel => self.close_provider_settings(),
                ProviderSettingsAction::StartCodexLogin => self.start_codex_login(),
                ProviderSettingsAction::RefreshModels => self
                    .provider_settings
                    .start_models_fetch_with_store(self.credential_store.clone()),
            }
        }
    }
}
