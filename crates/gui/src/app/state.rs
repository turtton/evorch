use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui_dock::DockState;
use workspace_ui::{Panel, PanelId, PanelKind, SidebarState, UiSettings};

use super::WorkbenchError;
use crate::diff::{DiffModel, DiffSource, GitCliDiffSource};
use crate::dock::to_dock_state;
use crate::events::EventPump;
use crate::keymap::Keymap;
use crate::model::codex_auth::CodexAuthModel;
use crate::model::commands::{
    CiStatus, CommandSink, FixtureLoopAdapter, GoalFormModel, LoopStatusView, MergeApprovalModel,
    MergeApprovalView, ReviewerStatus, WorkbenchCommand,
};
use crate::model::composer::{ComposerModel, ProviderStatus};
use crate::model::provider_settings::ProviderSettingsModel;
use crate::model::tasks::{AgentRunSource, TasksModel};
use crate::model::telemetry::TelemetryOverlay;
use crate::model::terminal::TerminalBuffer;
use crate::model::transcript::TranscriptModel;
use crate::model::transcript_registry::TranscriptRegistry;
use crate::pty::PtySession;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationFocus {
    Thread,
    Agent(String),
}

/// フレームごとにイベント・レイアウト・描画を統合する状態です。
pub struct WorkbenchState<S> {
    pub(super) attention_acks: BTreeMap<(PanelId, String), super::attention::ack::AttentionAck>,
    pub(super) external_job: Option<super::external_commands::Job>,
    pub(super) arena: crate::panes::arena::ArenaPane,
    pub(super) memory: crate::panes::memory::MemoryPane,
    pub(super) ownership: Option<Arc<runtime::ownership::OwnerHost>>,
    pub(super) ownership_error: Option<String>,
    pub(super) shutdown_requested: bool,
    pub(super) shutdown_confirmed: bool,
    pub(super) readonly_threads: std::collections::BTreeSet<String>,
    pub(super) pump: Option<EventPump>,
    pub(super) transcripts: TranscriptRegistry,
    pub(super) telemetry: TelemetryOverlay,
    pub(super) tasks: TasksModel<S>,
    pub(super) terminal: TerminalBuffer,
    pub(super) pty: Option<PtySession>,
    pub(super) dock: DockState<PanelId>,
    pub(super) panels: BTreeMap<PanelId, Panel>,
    pub(super) keymap: Keymap,
    pub(super) terminal_input: String,
    pub(super) save_path: Option<PathBuf>,
    pub(super) sidebar: SidebarState,
    pub(super) sidebar_path: Option<PathBuf>,
    pub(super) history: Vec<super::history::UserMessage>,
    pub(super) home_dir: Option<PathBuf>,
    pub(super) folder_picker: crate::model::folder_picker::FolderPickerModel,
    pub(super) focus: ConversationFocus,
    pub(super) theme_installed: bool,
    pub(super) theme_preset: crate::theme::style::ThemePreset,
    pub(super) diff: DiffModel,
    pub(super) diff_source: Arc<dyn DiffSource>,
    pub(super) goal_form: GoalFormModel,
    pub(super) composer: ComposerModel,
    pub(super) model_picker: crate::model::model_picker::ModelPickerState,
    pub(super) provider_status: ProviderStatus,
    pub(super) provider_settings: ProviderSettingsModel,
    pub(super) codex_auth: CodexAuthModel,
    pub(super) provider_settings_path: Option<PathBuf>,
    pub(super) credential_store: Option<Arc<dyn sandbox::CredentialStore>>,
    pub(super) provider_save_rx: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    pub(super) production_model: Option<(
        crate::model::production::ProductionModel,
        Arc<runtime::compose::SwitchableModel>,
    )>,
    pub(super) merge: MergeApprovalModel,
    pub(super) loop_status: LoopStatusView,
    pub(super) sink: Box<dyn CommandSink>,
    pub(super) issued: Vec<WorkbenchCommand>,
    pub(super) phases: BTreeMap<String, workspace_ui::ThreadRunPhase>,
}

impl<S: AgentRunSource> WorkbenchState<S> {
    pub fn new(source: S, settings: &UiSettings) -> Result<Self, WorkbenchError> {
        let workspace = settings.layout.workspace.clone().unwrap_or_default();
        workspace
            .validate()
            .map_err(WorkbenchError::InvalidWorkspace)?;
        let mut dock = to_dock_state(&workspace)?;
        crate::dock::enforce_sidebar_min_fraction(&mut dock, &workspace);
        let mut state = Self {
            attention_acks: BTreeMap::new(),
            external_job: None,
            arena: crate::panes::arena::ArenaPane::default(),
            memory: crate::panes::memory::MemoryPane::default(),
            ownership: None,
            ownership_error: None,
            shutdown_requested: false,
            shutdown_confirmed: false,
            readonly_threads: std::collections::BTreeSet::new(),
            pump: None,
            transcripts: TranscriptRegistry::new(),
            telemetry: TelemetryOverlay::new(),
            tasks: TasksModel::new(source),
            terminal: TerminalBuffer::new(10_000),
            pty: None,
            dock,
            panels: workspace.panels,
            keymap: Keymap::from_settings(&settings.keybinds),
            terminal_input: String::new(),
            save_path: None,
            sidebar: SidebarState::default(),
            sidebar_path: None,
            history: Vec::new(),
            home_dir: std::env::home_dir(),
            folder_picker: crate::model::folder_picker::FolderPickerModel::default(),
            focus: ConversationFocus::Thread,
            theme_installed: false,
            theme_preset: crate::theme::style::ThemePreset::Graphite,
            diff: DiffModel::new(),
            diff_source: Arc::new(GitCliDiffSource),
            goal_form: GoalFormModel::default(),
            composer: ComposerModel::default(),
            model_picker: crate::model::model_picker::ModelPickerState::default(),
            provider_status: ProviderStatus::default(),
            provider_settings: ProviderSettingsModel::default(),
            codex_auth: CodexAuthModel::default(),
            provider_settings_path: None,
            credential_store: None,
            provider_save_rx: None,
            production_model: None,
            merge: MergeApprovalModel {
                view: MergeApprovalView {
                    pr: None,
                    ci: CiStatus::Unknown,
                    reviewer: ReviewerStatus::Unknown,
                    diff_summary: None,
                    resolution: None,
                    binding: None,
                    gate: Vec::new(),
                    blocked: None,
                },
            },
            loop_status: LoopStatusView::default(),
            sink: Box::new(FixtureLoopAdapter::default()),
            issued: Vec::new(),
            phases: BTreeMap::new(),
        };
        state.tasks.refresh();
        Ok(state)
    }

    pub fn with_pump(mut self, pump: EventPump) -> Self {
        self.pump = Some(pump);
        self
    }

    pub fn with_memory_storage(mut self, config: storage::StorageConfig) -> Self {
        self.memory.config = Some(config);
        for (id, kind) in [
            ("memory-main", PanelKind::Memory),
            ("tasks-main", PanelKind::Tasks),
            ("arena-main", PanelKind::Arena),
        ] {
            let id = PanelId::new(id);
            if self.dock.find_tab(&id).is_none() {
                self.panels.insert(
                    id.clone(),
                    Panel {
                        id: id.clone(),
                        kind,
                        title: kind.default_title().into(),
                        target: None,
                    },
                );
                self.dock.push_to_focused_leaf(id);
            }
        }
        self
    }

    pub fn with_pty(mut self, pty: PtySession) -> Self {
        self.pty = Some(pty);
        self
    }

    pub fn with_save_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.save_path = Some(path.into());
        self
    }

    pub fn with_sidebar(mut self, sidebar: SidebarState) -> Self {
        self.transcripts
            .select_thread(sidebar.active_thread.as_ref().map(ToString::to_string));
        for thread in &sidebar.threads {
            for run in &thread.run_ids {
                self.transcripts.bind_run(run, &thread.id.to_string());
            }
        }
        self.sidebar = sidebar;
        self
    }

    pub fn with_sidebar_path(mut self, path: PathBuf) -> Self {
        self.sidebar_path = Some(path);
        self
    }

    pub fn with_home_dir(mut self, home: PathBuf) -> Self {
        self.home_dir = Some(home);
        self
    }

    pub fn with_folder_picker(
        mut self,
        picker: Arc<dyn crate::model::folder_picker::FolderPicker>,
    ) -> Self {
        self.folder_picker = crate::model::folder_picker::FolderPickerModel::new(picker);
        self
    }

    pub fn with_diff_source(mut self, source: Arc<dyn DiffSource>) -> Self {
        self.diff_source = source;
        self
    }

    pub fn with_command_sink(mut self, sink: Box<dyn CommandSink>) -> Self {
        self.sink = sink;
        self
    }

    pub fn reload_theme(&mut self, ctx: &egui::Context, preset: crate::theme::style::ThemePreset) {
        self.theme_preset = preset;
        crate::theme::style::install_preset(ctx, preset);
        self.theme_installed = true;
    }

    pub const fn dock(&self) -> &DockState<PanelId> {
        &self.dock
    }
    pub const fn dock_mut(&mut self) -> &mut DockState<PanelId> {
        &mut self.dock
    }
    pub const fn transcripts(&self) -> &TranscriptRegistry {
        &self.transcripts
    }
    pub const fn telemetry(&self) -> &TelemetryOverlay {
        &self.telemetry
    }
    pub const fn tasks(&self) -> &TasksModel<S> {
        &self.tasks
    }
    pub const fn terminal(&self) -> &TerminalBuffer {
        &self.terminal
    }
    pub const fn sidebar(&self) -> &SidebarState {
        &self.sidebar
    }
    pub const fn focus(&self) -> &ConversationFocus {
        &self.focus
    }
    pub const fn diff(&self) -> &DiffModel {
        &self.diff
    }
    pub const fn goal_form(&self) -> &GoalFormModel {
        &self.goal_form
    }
    pub const fn goal_form_mut(&mut self) -> &mut GoalFormModel {
        &mut self.goal_form
    }
    pub fn with_provider_status(mut self, status: ProviderStatus) -> Self {
        self.provider_status = status;
        self
    }
    pub const fn composer(&self) -> &ComposerModel {
        &self.composer
    }
    pub const fn composer_mut(&mut self) -> &mut ComposerModel {
        &mut self.composer
    }
    pub const fn provider_status(&self) -> &ProviderStatus {
        &self.provider_status
    }
    pub fn with_provider_settings(mut self, model: ProviderSettingsModel) -> Self {
        self.provider_settings = model;
        self
    }
    pub fn with_credential_store(mut self, store: Arc<dyn sandbox::CredentialStore>) -> Self {
        self.credential_store = Some(store);
        self
    }
    pub fn with_codex_auth(mut self, model: CodexAuthModel) -> Self {
        self.codex_auth = model;
        self
    }
    pub fn codex_auth(&self) -> &CodexAuthModel {
        match &self.provider_settings.editor {
            Some(crate::model::provider_settings::ProfileEditor::Codex(editor)) => &editor.auth,
            _ => &self.codex_auth,
        }
    }
    pub fn codex_auth_mut(&mut self) -> &mut CodexAuthModel {
        match &mut self.provider_settings.editor {
            Some(crate::model::provider_settings::ProfileEditor::Codex(editor)) => &mut editor.auth,
            _ => &mut self.codex_auth,
        }
    }
    pub fn with_provider_settings_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.provider_settings_path = Some(path.into());
        self
    }
    pub const fn provider_settings(&self) -> &ProviderSettingsModel {
        &self.provider_settings
    }
    pub const fn provider_settings_mut(&mut self) -> &mut ProviderSettingsModel {
        &mut self.provider_settings
    }
    pub fn provider_settings_path(&self) -> Option<&Path> {
        self.provider_settings_path.as_deref()
    }
    pub const fn merge(&self) -> &MergeApprovalModel {
        &self.merge
    }
    pub const fn loop_status(&self) -> &LoopStatusView {
        &self.loop_status
    }
    pub const fn thread_phases(&self) -> &BTreeMap<String, workspace_ui::ThreadRunPhase> {
        &self.phases
    }
    pub fn issued(&self) -> &[WorkbenchCommand] {
        &self.issued
    }
    pub fn save_path(&self) -> Option<&PathBuf> {
        self.save_path.as_ref()
    }

    pub fn transcript(&self) -> &TranscriptModel {
        match &self.focus {
            ConversationFocus::Thread => self.transcripts.thread(),
            ConversationFocus::Agent(run_id) => self
                .transcripts
                .run(run_id)
                .unwrap_or_else(|| self.transcripts.thread()),
        }
    }
}
