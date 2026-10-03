use egui_dock::DockArea;

use super::WorkbenchState;
use super::tab_viewer::WorkbenchTabViewer;
use crate::model::tasks::AgentRunSource;
use crate::panes::{
    agents::AgentsAction,
    composer::ComposerAction,
    notifications::NotificationsAction,
    provider_settings::{ProviderSettingsAction, provider_settings_modal},
    requests::RequestAction,
    sidebar::{SidebarAction, set_sidebar_error},
    tasks::TasksAction,
};

impl<S: AgentRunSource> WorkbenchState<S> {
    fn open_notification_thread(&mut self, ctx: &egui::Context, thread_id: workspace_ui::ThreadId) {
        let result = self.switch_thread(thread_id);
        if result.is_ok() {
            self.return_to_thread();
            self.focus_panel("agent-main");
        }
        set_sidebar_error(ctx, result.err().map(|error| error.to_string()));
    }

    pub(super) fn render(&mut self, ui: &mut egui::Ui) {
        self.poll_role_save();
        self.refresh_image_capability();
        self.composer.running_children = self
            .sidebar
            .active_thread
            .as_ref()
            .map(|id| {
                let thread = id.to_string();
                if let Some(count) = self.sink.running_children(&thread) {
                    self.running_children.insert(thread.clone(), count);
                }
                self.running_children.get(&thread).copied().unwrap_or(0)
            })
            .unwrap_or(0);
        self.composer.follow_ups = self
            .sidebar
            .active_thread
            .as_ref()
            .and_then(|id| self.sink.follow_up_status(&id.to_string()))
            .unwrap_or_default();
        self.composer.resolved_model = self
            .sidebar
            .threads
            .iter()
            .find(|thread| Some(&thread.id) == self.sidebar.active_thread.as_ref())
            .filter(|thread| thread.model_preference.is_none())
            .and(self.production_model.as_ref())
            .map(|(_, model)| {
                let role = match self.composer.role {
                    crate::model::composer::ComposerRole::Worker => runtime::Role::Worker,
                    crate::model::composer::ComposerRole::Orchestrator => {
                        runtime::Role::Orchestrator
                    }
                };
                runtime::AgentModel::selected_model(model.as_ref(), role, None)
            });
        egui::Panel::bottom("workbench-footer")
            .resizable(false)
            .frame(egui::Frame::NONE.inner_margin(egui::vec2(6.0, 3.0)))
            .show(ui, |ui| self.footer_ui(ui));
        let inactive_runs: std::collections::BTreeSet<_> = self
            .sidebar
            .threads
            .iter()
            .filter(|thread| Some(&thread.id) != self.sidebar.active_thread.as_ref())
            .flat_map(|thread| thread.run_ids.iter().cloned())
            .collect();
        let mut subagent_closed = false;
        self.panels.retain(|panel_id, panel| {
            if panel.kind == workspace_ui::PanelKind::FileViewer
                && self.dock.find_tab(panel_id).is_none()
            {
                if let Some(path) = panel.target.as_deref() {
                    crate::panes::file_viewer::forget_file(ui.ctx(), std::path::Path::new(path));
                }
                return false;
            }
            let retained = !panel_id.as_str().starts_with("agent-run-")
                || self.dock.find_tab(panel_id).is_some()
                || panel
                    .target
                    .as_ref()
                    .is_some_and(|run| inactive_runs.contains(run));
            subagent_closed |= !retained
                && matches!(
                    panel.kind,
                    workspace_ui::PanelKind::SubagentTranscript
                        | workspace_ui::PanelKind::ParkedAgentTranscript(_)
                );
            retained
        });
        if subagent_closed {
            self.equalize_subagent_panes();
        }
        let ctx = ui.ctx().clone();
        self.observe_attention();
        let mut sidebar_action = None;
        let mut agents_action = None;
        let mut tasks_action = None;
        let mut notifications_action = None;
        let mut request_action = None;
        let mut diagnostics_request = false;
        let mut diff_request = None;
        let mut file_requests = Vec::new();
        let mut composer_action = None;
        let mut focus_request = None;
        let mut preference_action = None;
        let profiles = self.available_profiles();
        let active_repo_root = self.active_repo_root();
        let dock_style = crate::theme::dock::dock_style(ui.style());
        let tab_style = dock_style.tab.clone();
        let sandbox_picker = crate::panes::composer::SandboxPickerContext {
            mode: self.sandbox_settings.config.escalation_approval,
            enabled: !self.settings_save_in_progress(),
        };
        {
            let mut viewer = WorkbenchTabViewer {
                sandbox_picker,
                pending_approvals: &self.pending_approvals,
                request_action: &mut request_action,
                diagnostics_request: &mut diagnostics_request,
                user_questions: &self.user_questions,
                question_drafts: &mut self.question_drafts,
                notifications: &mut self.notifications,
                notifications_action: &mut notifications_action,
                attention_acks: &mut self.attention_acks,
                memory: &mut self.memory,
                self_improvement: &mut self.self_improvement,
                arena: &mut self.arena,
                transcripts: &self.transcripts,
                ledger: &self.ledger,
                telemetry: &self.telemetry,
                tasks: &mut self.tasks,
                durable_tasks: &self.durable_tasks,
                selected_task: self.selected_task.as_deref(),
                tasks_action: &mut tasks_action,
                terminal: &mut self.terminal,
                terminal_input: &mut self.terminal_input,
                pty: &mut self.pty,
                panels: &self.panels,
                sidebar: &self.sidebar,
                phases: &self.phases,
                sidebar_action: &mut sidebar_action,
                agents_action: &mut agents_action,
                focus: &self.focus,
                diff: &mut self.diff,
                diff_source: &self.diff_source,
                diff_request: &mut diff_request,
                file_requests: &mut file_requests,
                composer: &mut self.composer,
                composer_action: &mut composer_action,
                focus_request: &mut focus_request,
                dock_tab_style: &tab_style,
                profiles: &profiles,
                picker_state: &mut self.model_picker,
                preference_action: &mut preference_action,
                repo_root: active_repo_root.as_deref(),
            };
            DockArea::new(&mut self.dock)
                .style(dock_style)
                // Leaf "close all" never closes anything: most panes are not closeable.
                .show_leaf_close_all_buttons(false)
                .show_inside(ui, &mut viewer);
        }
        for link in file_requests {
            self.open_file_preview(&ctx, link);
        }
        if diagnostics_request {
            self.diagnostics_open = true;
            self.restore_status = None;
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
                AgentsAction::StopRun(run_id) => {
                    if let Some(thread_id) = self.sidebar.active_thread.as_ref() {
                        self.submit_command(crate::model::commands::WorkbenchCommand::StopRun {
                            thread_id: thread_id.to_string(),
                            run_id,
                        });
                    }
                }
                AgentsAction::OpenTask(task_id) => {
                    // Every navigation should reveal the target, including repeated visits.
                    ctx.data_mut(|data| {
                        data.remove::<String>(egui::Id::new("tasks_scrolled_selection"));
                    });
                    self.selected_task = Some(task_id);
                    self.focus_panel("tasks-main");
                }
            }
        }
        if let Some(TasksAction::OpenRun(run_id)) = tasks_action {
            self.open_agent_pane(&run_id);
        }
        if let Some(action) = notifications_action {
            match action {
                NotificationsAction::OpenRun(run_id) => self.open_agent_pane(&run_id),
                NotificationsAction::OpenConversation(run_id) => {
                    if let Some(thread_id) = self.thread_for_run(&run_id) {
                        self.open_notification_thread(&ctx, workspace_ui::ThreadId::new(thread_id));
                    }
                }
                NotificationsAction::OpenThread(thread_id) => {
                    self.open_notification_thread(&ctx, thread_id);
                }
            }
        }
        if let Some(action) = request_action {
            match action {
                RequestAction::Decide { call_id, approved } => {
                    self.decide_tool_approval(call_id, approved)
                }
                RequestAction::Answer {
                    thread_id,
                    question_id,
                    answer,
                } => {
                    self.submit_command(
                        crate::model::commands::WorkbenchCommand::AnswerUserQuestion {
                            thread_id,
                            question_id,
                            answer,
                        },
                    );
                }
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
                SidebarAction::SetPrimaryProject(project_id) => {
                    self.set_primary_project(project_id)
                }
                SidebarAction::AddProject(path) => self.add_project(path).map(|_| ()),
                SidebarAction::CreateThread(title) => self.create_thread(title).map(|_| ()),
                SidebarAction::ForkThread(thread_id) => self.fork_thread(thread_id).map(|_| ()),
                SidebarAction::SwitchThread(thread_id) => self.switch_thread(thread_id),
                SidebarAction::TogglePin(thread_id) => self.toggle_pin(thread_id),
                SidebarAction::ToggleArchive(thread_id) => self.toggle_archive(thread_id),
                SidebarAction::SetTrust { path, trust } => self.set_allowed_trust(path, trust),
            };
            set_sidebar_error(&ctx, result.err().map(|error| error.to_string()));
        }
        if let Some(action) = composer_action {
            match action {
                ComposerAction::Send => self.submit_composer(),
                ComposerAction::Stop => self.stop_chat(),
                ComposerAction::DeliverNextTurn => self.deliver_follow_ups_next_turn(),
                ComposerAction::Discard => self.cancel_chat(),
                ComposerAction::ModelPreference(_) => {}
                ComposerAction::OpenSandboxSettings => self.open_sandbox_settings(),
                ComposerAction::OpenSelfImprovementSettings => {
                    self.open_self_improvement_settings()
                }
                ComposerAction::OpenStorageSettings => self.open_storage_settings(),
                ComposerAction::Complete(name) => {
                    self.composer_mut().input = format!("/{name} ");
                }
                ComposerAction::CompleteExternal(name) => {
                    self.composer_mut().input = format!("/{name} ");
                }
            }
        }
        self.render_theme_settings(ui.ctx());
        self.render_sandbox_settings(ui.ctx());
        self.render_self_improvement_settings(ui.ctx());
        self.render_storage_settings(ui.ctx());
        if self.routing_settings.open {
            use crate::panes::routing_settings::{RoutingSettingsAction, routing_settings_modal};
            match routing_settings_modal(ui.ctx(), &mut self.routing_settings) {
                Some(RoutingSettingsAction::Save) => self.submit_routing_settings(),
                Some(RoutingSettingsAction::Cancel) => self.routing_settings.open = false,
                None => {}
            }
        }
        self.render_role_settings(ui.ctx());
        if self.provider_settings.open
            && !self.role_settings.open
            && !self.routing_settings.open
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

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Open a local file in the workspace, reusing an existing tab for that path.
    pub fn open_file_preview(
        &mut self,
        ctx: &egui::Context,
        link: crate::panes::file_viewer::FileLink,
    ) {
        use workspace_ui::{Panel, PanelId, PanelKind};
        let target = link.path.to_string_lossy().into_owned();
        let id = PanelId::new(format!("file-{target}"));
        crate::panes::file_viewer::request_line(ctx, &link);
        self.panels.entry(id.clone()).or_insert_with(|| Panel {
            id: id.clone(),
            kind: PanelKind::FileViewer,
            title: link.path.file_name().map_or_else(
                || target.clone(),
                |name| name.to_string_lossy().into_owned(),
            ),
            target: Some(target),
        });
        if self.dock.find_tab(&id).is_none() {
            let neighbor = self
                .dock
                .find_tab(&PanelId::new("subagents-home"))
                .or_else(|| self.dock.find_tab(&PanelId::new("agent-main")));
            if let Some(path) = neighbor {
                self.dock.set_focused_node_and_surface(path.node_path());
            }
            self.dock.push_to_focused_leaf(id.clone());
        }
        self.focus_panel(id.as_str());
        ctx.request_repaint();
    }
}
