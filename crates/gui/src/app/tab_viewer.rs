use std::collections::BTreeMap;
use std::path::Path;

use egui_dock::TabViewer;
use workspace_ui::{Panel, PanelId, PanelKind, SidebarState};

use super::ConversationFocus;
use super::attention::{PaneAttention, ack::AttentionAck, acknowledged_attention};
use crate::diff::{DiffMode, DiffModel};
use crate::model::composer::ComposerModel;
use crate::model::notifications::NotificationsModel;
use crate::model::pending_approvals::PendingApprovalsModel;
use crate::model::tasks::{AgentRunSource, TasksModel};
use crate::model::telemetry::TelemetryOverlay;
use crate::model::terminal::TerminalBuffer;
use crate::model::transcript_registry::TranscriptRegistry;
use crate::panes::{
    agent_transcript::agent_transcript_pane_with_repo_root,
    agents::{AgentsAction, subagents_pane},
    composer::ComposerAction,
    diff::diff_pane,
    file_viewer::{FileLink, file_viewer_pane, take_file_links},
    notifications::{NotificationsAction, notifications_pane},
    requests::RequestAction,
    sidebar::{SidebarAction, sidebar_pane},
    tasks::{TasksAction, tasks_pane},
    terminal::terminal_pane,
};
use crate::pty::PtySession;

mod conversation;

pub(super) struct WorkbenchTabViewer<'a, S> {
    pub(super) pending_approvals: &'a PendingApprovalsModel,
    pub(super) request_action: &'a mut Option<RequestAction>,
    pub(super) diagnostics_request: &'a mut bool,
    pub(super) user_questions: &'a BTreeMap<String, event_bus::UserQuestion>,
    pub(super) question_drafts: &'a mut BTreeMap<String, String>,
    pub(super) notifications: &'a mut NotificationsModel,
    pub(super) notifications_action: &'a mut Option<NotificationsAction>,
    pub(super) attention_acks: &'a mut BTreeMap<(PanelId, String), AttentionAck>,
    pub(super) arena: &'a mut crate::panes::arena::ArenaPane,
    pub(super) memory: &'a mut crate::panes::memory::MemoryPane,
    pub(super) transcripts: &'a TranscriptRegistry,
    pub(super) ledger: &'a crate::model::ledger::LedgerRegistry,
    pub(super) telemetry: &'a TelemetryOverlay,
    pub(super) tasks: &'a mut TasksModel<S>,
    pub(super) durable_tasks: &'a crate::model::durable_tasks::DurableTasksModel,
    pub(super) selected_task: Option<&'a str>,
    pub(super) tasks_action: &'a mut Option<TasksAction>,
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
    pub(super) file_requests: &'a mut Vec<FileLink>,
    pub(super) diff_request: &'a mut Option<DiffMode>,
    pub(super) composer: &'a mut ComposerModel,
    pub(super) composer_action: &'a mut Option<ComposerAction>,
    pub(super) focus_request: &'a mut Option<&'static str>,
    pub(super) dock_tab_style: &'a egui_dock::TabStyle,
    pub(super) profiles: &'a [runtime::compose::ProfileSummary],
    pub(super) picker_state: &'a mut crate::model::model_picker::ModelPickerState,
    pub(super) preference_action: &'a mut Option<Option<workspace_ui::ModelPreference>>,
    pub(super) repo_root: Option<&'a Path>,
    pub(super) sandbox_picker: crate::panes::composer::SandboxPickerContext,
}

impl<S: AgentRunSource> WorkbenchTabViewer<'_, S> {
    fn attention_for_tab(&self, tab: &PanelId) -> PaneAttention {
        if tab.as_str() == "notifications-main" {
            return if self.notifications.unread_count() > 0 {
                PaneAttention::Info
            } else {
                PaneAttention::None
            };
        }
        acknowledged_attention(self.attention_acks, tab)
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
            .map(|panel| {
                let owner = match panel.kind {
                    PanelKind::SubagentTranscript | PanelKind::ParkedAgentTranscript(_) => {
                        panel.target.as_ref().and_then(|run| {
                            self.sidebar
                                .threads
                                .iter()
                                .find(|thread| thread.run_ids.contains(run))
                        })
                    }
                    PanelKind::Agent
                    | PanelKind::AgentTranscript
                    | PanelKind::SubagentRegion
                    | PanelKind::Sidebar
                    | PanelKind::Agents
                    | PanelKind::Notifications
                    | PanelKind::FileViewer
                    | PanelKind::Diff
                    | PanelKind::Terminal
                    | PanelKind::Tasks
                    | PanelKind::Memory
                    | PanelKind::Arena => None,
                };
                match owner {
                    Some(thread) => format!("{} · {}", panel.title, thread.id),
                    None => panel.title.clone(),
                }
            })
            .unwrap_or_else(|| tab.to_string());
        // egui_dock paints tab titles without accesskit nodes, so the "• " prefix
        // never reaches label-based test queries; the tab title text is unchanged.
        // U+2022 is used because egui's bundled fonts lack U+25CF (renders as tofu).
        match self.attention_for_tab(tab).color() {
            Some(color) => egui::RichText::new(format!("• {title}"))
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

    // Each pane owns its scroll viewport. An outer dock ScrollArea gives the
    // conversation unbounded width and captures wheel input across nested panes.
    fn scroll_bars(&self, _tab: &Self::Tab) -> [bool; 2] {
        [false, false]
    }

    fn is_closeable(&self, tab: &Self::Tab) -> bool {
        tab.as_str().starts_with("agent-run-")
            || self
                .panels
                .get(tab)
                .is_some_and(|panel| panel.kind == PanelKind::FileViewer)
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        ui.set_clip_rect(ui.clip_rect().intersect(ui.max_rect()));
        let Some(panel) = self.panels.get(tab) else {
            return;
        };
        let command_start = ui.ctx().output(|output| output.commands.len());
        let displayed: Vec<_> = self
            .attention_acks
            .iter()
            .filter(|((id, _), _)| id == tab)
            .map(|(key, ack)| (key.clone(), ack.revision()))
            .collect();
        let surface_visible = ui.is_visible() && ui.clip_rect().intersects(ui.max_rect());
        match panel.kind {
            PanelKind::SubagentRegion | PanelKind::Agents => {
                let thread = self
                    .sidebar
                    .threads
                    .iter()
                    .find(|thread| Some(&thread.id) == self.sidebar.active_thread.as_ref());
                if let Some(action) = subagents_pane(
                    ui,
                    self.tasks,
                    self.telemetry,
                    self.durable_tasks,
                    thread
                        .map(|thread| thread.run_ids.as_slice())
                        .unwrap_or_default(),
                ) {
                    *self.agents_action = Some(action);
                }
            }
            PanelKind::Agent => self.agent_tab_ui(ui, tab),
            PanelKind::Sidebar => {
                if let Some(action) = sidebar_pane(ui, self.sidebar, self.phases, self.telemetry) {
                    *self.sidebar_action = Some(action);
                }
            }
            PanelKind::Notifications => {
                let focused = ui.input(|input| input.viewport().focused);
                if let Some(action) = notifications_pane(ui, self.notifications, focused) {
                    *self.notifications_action = Some(action);
                }
            }
            PanelKind::AgentTranscript
            | PanelKind::SubagentTranscript
            | PanelKind::ParkedAgentTranscript(_) => {
                let run_id = panel.target.as_deref().unwrap_or_default();
                if let Some(ack) = self.attention_acks.get(&(tab.clone(), run_id.to_owned())) {
                    crate::panes::phase_indicator::phase_indicator_with_ack(
                        ui,
                        ack.phase(),
                        ack.is_unread(),
                    );
                }
                agent_transcript_pane_with_repo_root(
                    ui,
                    run_id,
                    self.transcripts.run(run_id),
                    self.repo_root,
                );
            }
            PanelKind::FileViewer => {
                if let Some(path) = panel.target.as_deref() {
                    file_viewer_pane(ui, Path::new(path));
                }
            }
            PanelKind::Diff => {
                if let Some(mode) = diff_pane(ui, self.diff) {
                    *self.diff_request = Some(mode);
                }
            }
            PanelKind::Terminal => terminal_pane(ui, self.terminal, self.terminal_input, self.pty),
            PanelKind::Tasks => {
                if let Some(action) = tasks_pane(
                    ui,
                    self.durable_tasks,
                    &self.tasks.teams(),
                    self.memory.config.as_ref(),
                    self.selected_task,
                ) {
                    *self.tasks_action = Some(action);
                }
            }
            PanelKind::Memory => {
                let project = self
                    .sidebar
                    .selected_project
                    .as_ref()
                    .map(ToString::to_string);
                self.memory.render(ui, project.as_deref());
            }
            PanelKind::Arena => {
                let project = self
                    .sidebar
                    .selected_project
                    .as_ref()
                    .map(ToString::to_string);
                self.arena
                    .render(ui, self.memory.config.as_ref().zip(project.as_deref()));
            }
        }
        let link_base = if panel.kind == PanelKind::FileViewer {
            panel
                .target
                .as_deref()
                .and_then(|path| Path::new(path).parent())
        } else {
            self.repo_root
        };
        self.file_requests
            .extend(take_file_links(ui.ctx(), command_start, link_base));
        if surface_visible {
            let focused = ui.input(|input| input.viewport().focused);
            for (key, revision) in displayed {
                if let Some(ack) = self.attention_acks.get_mut(&key) {
                    let unread = ack.is_unread();
                    if ack.acknowledge_surface(Some(&revision), focused) && unread {
                        ui.ctx().request_repaint();
                    }
                }
            }
        }
    }
}
