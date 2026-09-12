use std::collections::BTreeMap;

use egui_dock::TabViewer;
use workspace_ui::{Panel, PanelId, PanelKind, SidebarState};

use super::ConversationFocus;
use super::attention::{PaneAttention, ack::AttentionAck, acknowledged_attention};
use crate::diff::{DiffMode, DiffModel};
use crate::model::composer::{ComposerModel, ProviderStatus};
use crate::model::tasks::{AgentRunSource, TasksModel};
use crate::model::telemetry::TelemetryOverlay;
use crate::model::terminal::TerminalBuffer;
use crate::model::transcript_registry::TranscriptRegistry;
use crate::panes::{
    agent::{AgentIdentity, AgentPaneAction, ConversationContext, agent_pane},
    agent_transcript::agent_transcript_pane,
    agents::{AgentsAction, agents_pane},
    composer::ComposerAction,
    diff::diff_pane,
    sidebar::{SidebarAction, sidebar_pane},
    tasks::tasks_pane,
    terminal::terminal_pane,
};
use crate::pty::PtySession;

pub(super) struct WorkbenchTabViewer<'a, S> {
    pub(super) attention_acks: &'a mut BTreeMap<(PanelId, String), AttentionAck>,
    pub(super) arena: &'a mut crate::panes::arena::ArenaPane,
    pub(super) memory: &'a mut crate::panes::memory::MemoryPane,
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
    pub(super) composer: &'a mut ComposerModel,
    pub(super) provider_status: &'a ProviderStatus,
    pub(super) composer_action: &'a mut Option<ComposerAction>,
    pub(super) focus_request: &'a mut Option<&'static str>,
    pub(super) dock_tab_style: &'a egui_dock::TabStyle,
    pub(super) profiles: &'a [runtime::compose::ProfileSummary],
    pub(super) picker_state: &'a mut crate::model::model_picker::ModelPickerState,
    pub(super) preference_action: &'a mut Option<Option<workspace_ui::ModelPreference>>,
}

impl<S: AgentRunSource> WorkbenchTabViewer<'_, S> {
    fn attention_for_tab(&self, tab: &PanelId) -> PaneAttention {
        acknowledged_attention(self.attention_acks, tab)
    }

    fn agent_tab_ui(&mut self, ui: &mut egui::Ui, tab: &PanelId) {
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
            phase_unread: self
                .attention_acks
                .iter()
                .any(|((id, _), ack)| id == tab && ack.is_unread()),
            has_project: self.sidebar.selected_project.is_some(),
            active_thread_title: active_thread.map(|thread| thread.title.as_str()),
            phase: self
                .attention_acks
                .iter()
                .find(|((id, _), _)| id == tab)
                .map(|(_, ack)| ack.phase()),
            next_thread_title: format!("thread-{}", self.sidebar.threads.len() + 1),
            model_picker: crate::panes::model_picker::ModelPickerContext {
                profiles: self.profiles,
                preference: active_thread.and_then(|thread| thread.model_preference.as_ref()),
                enabled: active_thread.is_some(),
            },
        };
        if let Some(action) = agent_pane(
            ui,
            transcript,
            identity,
            ctx,
            self.composer,
            self.provider_status,
            self.picker_state,
        ) {
            match action {
                AgentPaneAction::Agents(a) => *self.agents_action = Some(a),
                AgentPaneAction::Sidebar(a) => *self.sidebar_action = Some(a),
                AgentPaneAction::FocusPanel(id) => *self.focus_request = Some(id),
                AgentPaneAction::Composer(a) => *self.composer_action = Some(a),
                AgentPaneAction::ModelPreference(preference) => {
                    *self.preference_action = Some(preference)
                }
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

    fn is_closeable(&self, tab: &Self::Tab) -> bool {
        tab.as_str().starts_with("agent-run-")
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        let Some(panel) = self.panels.get(tab) else {
            return;
        };
        let displayed: Vec<_> = self
            .attention_acks
            .iter()
            .filter(|((id, _), _)| id == tab)
            .map(|(key, ack)| (key.clone(), ack.revision()))
            .collect();
        let surface_visible = ui.is_visible() && ui.clip_rect().intersects(ui.max_rect());
        match panel.kind {
            PanelKind::Agent => self.agent_tab_ui(ui, tab),
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
                if let Some(ack) = self.attention_acks.get(&(tab.clone(), run_id.to_owned())) {
                    crate::panes::phase_indicator::phase_indicator_with_ack(
                        ui,
                        ack.phase(),
                        ack.is_unread(),
                    );
                }
                agent_transcript_pane(ui, run_id, self.transcripts.run(run_id));
            }
            PanelKind::Diff => {
                if let Some(mode) = diff_pane(ui, self.diff) {
                    *self.diff_request = Some(mode);
                }
            }
            PanelKind::Terminal => terminal_pane(ui, self.terminal, self.terminal_input, self.pty),
            PanelKind::Tasks => {
                crate::panes::team::team_pane(ui, &self.tasks.teams());
                ui.separator();
                if let Some(config) = &self.memory.config {
                    crate::panes::tasks::dependencies_pane(ui, config);
                    ui.separator();
                }
                tasks_pane(ui, self.tasks);
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
