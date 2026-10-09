// allow: SIZE_OK - #110 は既存の状態所有・初期化 API への追加に限定し、状態全体の分割は別変更とする。
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
use crate::model::notifications::NotificationsModel;
use crate::model::pending_approvals::PendingApprovalsModel;
use crate::model::provider_settings::ProviderSettingsModel;
use crate::model::tasks::{AgentRunSource, TasksModel};
use crate::model::telemetry::TelemetryOverlay;
use crate::model::transcript::TranscriptModel;
use crate::model::transcript_registry::TranscriptRegistry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationFocus {
    Thread,
    Agent(String),
}

/// フレームごとにイベント・レイアウト・描画を統合する状態です。
pub struct WorkbenchState<S> {
    pub(super) auto_title_jobs: Vec<super::auto_title::Job>,
    pub(super) title_generator: Option<Arc<super::auto_title::TitleGenerator>>,
    pub(super) manually_titled: std::collections::BTreeSet<workspace_ui::ThreadId>,
    pub(super) attention_acks: BTreeMap<(PanelId, String), super::attention::ack::AttentionAck>,
    pub(super) notifications: NotificationsModel,
    pub(super) system_notifications: super::system_notifications::SystemNotifications,
    pub(super) pending_approvals: PendingApprovalsModel,
    pub(super) user_questions: BTreeMap<String, event_bus::UserQuestion>,
    pub(super) question_drafts: BTreeMap<String, String>,
    pub(super) diagnostics_open: bool,
    pub(super) diagnostics_run: String,
    pub(super) restore_status:
        Option<Result<Option<runtime::restore::RunRestoreDiagnostics>, String>>,
    pub(super) external_job: Option<super::external_commands::Job>,
    pub(super) mention_job: Option<super::mentions::Job>,
    /// Whether the composer had an `@` query last frame; a new one refreshes the index.
    pub(super) mention_active: bool,
    pub(super) arena: crate::panes::arena::ArenaPane,
    pub(super) usage: crate::panes::usage::UsagePane,
    pub(super) context_inspector: crate::panes::context_inspector::ContextInspectorPane,
    pub(super) shell_jobs: crate::panes::shell_jobs::ShellJobsPane,
    pub(super) memory: crate::panes::memory::MemoryPane,
    pub(super) self_improvement: crate::panes::self_improvement::SelfImprovementPane,
    pub(super) ownership: Option<Arc<runtime::ownership::OwnerHost>>,
    pub(super) ownership_status: super::ownership_status::OwnershipStatus,
    pub(super) ownership_action_error: Option<super::ownership::OwnershipActionError>,
    pub(super) shutdown_requested: bool,
    pub(super) shutdown_confirmed: bool,
    pub(super) close_in_flight: bool,
    pub(super) readonly_threads: std::collections::BTreeSet<String>,
    pub(super) pump: Option<EventPump>,
    pub(super) last_slow_frame_log: Option<std::time::Instant>,
    pub(super) transcripts: TranscriptRegistry,
    pub(super) ledger: crate::model::ledger::LedgerRegistry,
    pub(super) telemetry: TelemetryOverlay,
    pub(super) tasks: TasksModel<S>,
    pub(super) durable_tasks: crate::model::durable_tasks::DurableTasksModel,
    pub(super) selected_task: Option<String>,
    pub(super) terminals: crate::terminal::TerminalSessions,
    pub(super) dock: DockState<PanelId>,
    pub(super) panels: BTreeMap<PanelId, Panel>,
    pub(super) keymap: Keymap,
    pub(super) save_path: Option<PathBuf>,
    pub(super) sidebar: SidebarState,
    pub(super) sidebar_path: Option<PathBuf>,
    pub(super) history: Vec<super::history::UserMessage>,
    pub(super) home_dir: Option<PathBuf>,
    pub(super) folder_picker: crate::model::folder_picker::FolderPickerModel,
    pub(super) artifact_opener: Option<Arc<dyn crate::model::artifact_opener::ArtifactOpener>>,
    pub(super) project_dialog: crate::model::project_dialog::ProjectDialog,
    pub(super) focus: ConversationFocus,
    pub(super) theme_installed: bool,
    pub(super) theme_preset: crate::theme::style::ThemePreset,
    pub(super) theme_settings: super::theme_settings::ThemeSettings,
    pub(super) diff: DiffModel,
    pub(super) diff_source: Arc<dyn DiffSource>,
    pub(super) goal_form: GoalFormModel,
    pub(super) composer: ComposerModel,
    pub(super) composer_attachments:
        BTreeMap<workspace_ui::ThreadId, Vec<crate::model::composer::ImageAttachment>>,
    pub(super) model_picker: crate::model::model_picker::ModelPickerState,
    pub(super) provider_status: ProviderStatus,
    pub(super) provider_settings: ProviderSettingsModel,
    pub(super) role_settings: crate::model::role_settings::RoleSettingsModel,
    pub(super) routing_settings: crate::model::routing_settings::RoutingSettingsModel,
    pub(super) sandbox_settings: super::sandbox_settings::SandboxSettings,
    pub(super) self_improvement_settings:
        crate::model::self_improvement_settings::SelfImprovementSettingsModel,
    pub(super) storage_settings: super::storage_settings::StorageSettings,
    pub(super) codex_auth: CodexAuthModel,
    pub(super) provider_settings_path: Option<PathBuf>,
    pub(super) settings_load_options: config::LoadOptions,
    pub(super) credential_store: Option<Arc<dyn sandbox::CredentialStore>>,
    pub(super) provider_save_rx: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    /// Recomposition after the active project switched role profile.
    pub(super) project_profile_rx: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    pub(super) production_model: Option<(
        crate::model::production::ProductionModel,
        Arc<runtime::compose::SwitchableModel>,
    )>,
    /// Models of projects other than the startup one, shared with the runtime.
    pub(super) project_models: Option<crate::model::production::ProjectModels>,
    pub(super) merge: MergeApprovalModel,
    pub(super) loop_status: LoopStatusView,
    pub(super) thread_goals: BTreeMap<String, event_bus::ThreadGoalSnapshot>,
    pub(super) thread_todos: BTreeMap<String, event_bus::ThreadTodoSnapshot>,
    pub(super) sink: Box<dyn CommandSink>,
    /// Project trust last declared to the sink, so only changes are re-sent.
    pub(super) declared_trust: std::collections::BTreeMap<PathBuf, workspace_ui::TrustState>,
    pub(super) issued: Vec<WorkbenchCommand>,
    pub(super) phases: BTreeMap<String, workspace_ui::ThreadRunPhase>,
    pub(super) running_children: BTreeMap<String, usize>,
    /// Display state each thread had last frame, to notice finished work.
    pub(super) thread_states: BTreeMap<workspace_ui::ThreadId, workspace_ui::ThreadState>,
    /// Threads that finished while not open; cleared once the user opens them.
    pub(super) unread_threads: std::collections::BTreeSet<workspace_ui::ThreadId>,
    /// Runs whose latest lifecycle event completed a turn: safe rewind points.
    pub(super) idle_turns: std::collections::BTreeSet<String>,
    pub(super) usage_ledger: Option<super::usage_ledger::UsageLedgerLink>,
}

impl<S: AgentRunSource> WorkbenchState<S> {
    pub const fn ledger(&self) -> &crate::model::ledger::LedgerRegistry {
        &self.ledger
    }

    pub fn new(source: S, settings: &UiSettings) -> Result<Self, WorkbenchError> {
        let workspace = settings.layout.workspace.clone().unwrap_or_default();
        workspace
            .validate()
            .map_err(WorkbenchError::InvalidWorkspace)?;
        let mut dock = to_dock_state(&workspace)?;
        crate::dock::enforce_sidebar_min_fraction(&mut dock, &workspace);
        if settings.layout.workspace.is_none()
            && let Some(path) = dock.find_tab(&PanelId::new("terminal-main"))
            && let Ok(leaf) = dock.leaf_mut(path.node_path())
        {
            leaf.collapsed = true;
        }
        let mut state = Self {
            auto_title_jobs: Vec::new(),
            title_generator: None,
            manually_titled: std::collections::BTreeSet::new(),
            attention_acks: BTreeMap::new(),
            notifications: NotificationsModel::default(),
            system_notifications: super::system_notifications::SystemNotifications::default(),
            pending_approvals: PendingApprovalsModel::default(),
            user_questions: BTreeMap::new(),
            question_drafts: BTreeMap::new(),
            diagnostics_open: false,
            diagnostics_run: String::new(),
            restore_status: None,
            external_job: None,
            mention_job: None,
            mention_active: false,
            arena: crate::panes::arena::ArenaPane::default(),
            usage: crate::panes::usage::UsagePane::default(),
            context_inspector: crate::panes::context_inspector::ContextInspectorPane::default(),
            shell_jobs: crate::panes::shell_jobs::ShellJobsPane::default(),
            memory: crate::panes::memory::MemoryPane::default(),
            self_improvement: crate::panes::self_improvement::SelfImprovementPane::default(),
            ownership: None,
            ownership_status: super::ownership_status::OwnershipStatus::default(),
            ownership_action_error: None,
            shutdown_requested: false,
            shutdown_confirmed: false,
            close_in_flight: false,
            readonly_threads: std::collections::BTreeSet::new(),
            pump: None,
            last_slow_frame_log: None,
            transcripts: TranscriptRegistry::new(),
            ledger: crate::model::ledger::LedgerRegistry::default(),
            telemetry: TelemetryOverlay::new(),
            tasks: TasksModel::new(source),
            durable_tasks: crate::model::durable_tasks::DurableTasksModel::default(),
            selected_task: None,
            terminals: crate::terminal::TerminalSessions::default(),
            dock,
            panels: workspace.panels,
            keymap: Keymap::from_settings(&settings.keybinds),
            save_path: None,
            sidebar: SidebarState::default(),
            sidebar_path: None,
            history: Vec::new(),
            home_dir: std::env::home_dir(),
            folder_picker: crate::model::folder_picker::FolderPickerModel::default(),
            artifact_opener: None,
            project_dialog: crate::model::project_dialog::ProjectDialog::default(),
            focus: ConversationFocus::Thread,
            theme_installed: false,
            theme_preset: settings.theme_preset.into(),
            theme_settings: super::theme_settings::ThemeSettings::new(settings.clone()),
            diff: DiffModel::new(),
            diff_source: Arc::new(GitCliDiffSource),
            goal_form: GoalFormModel::default(),
            composer: ComposerModel::default(),
            composer_attachments: BTreeMap::new(),
            model_picker: crate::model::model_picker::ModelPickerState::default(),
            provider_status: ProviderStatus::default(),
            provider_settings: ProviderSettingsModel::default(),
            role_settings: crate::model::role_settings::RoleSettingsModel::default(),
            routing_settings: crate::model::routing_settings::RoutingSettingsModel::default(),
            sandbox_settings: super::sandbox_settings::SandboxSettings::default(),
            self_improvement_settings:
                crate::model::self_improvement_settings::SelfImprovementSettingsModel::default(),
            storage_settings: super::storage_settings::StorageSettings::default(),
            codex_auth: CodexAuthModel::default(),
            provider_settings_path: None,
            settings_load_options: config::LoadOptions::default(),
            credential_store: None,
            provider_save_rx: None,
            project_profile_rx: None,
            production_model: None,
            project_models: None,
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
            thread_goals: BTreeMap::new(),
            thread_todos: BTreeMap::new(),
            sink: Box::new(FixtureLoopAdapter::default()),
            declared_trust: std::collections::BTreeMap::new(),
            issued: Vec::new(),
            phases: BTreeMap::new(),
            running_children: BTreeMap::new(),
            thread_states: BTreeMap::new(),
            unread_threads: std::collections::BTreeSet::new(),
            idle_turns: std::collections::BTreeSet::new(),
            usage_ledger: None,
        };
        // Older saved layouts contain the former automatically registered tab.
        let arena = PanelId::new("arena-main");
        if state
            .panels
            .get(&arena)
            .is_some_and(|panel| panel.kind == PanelKind::Arena)
        {
            while let Some(path) = state.dock.find_tab(&arena) {
                state.dock.remove_tab(path);
            }
            state.panels.remove(&arena);
        }
        state.tasks.refresh();
        state.register_work_panels();
        Ok(state)
    }

    pub fn with_pump(mut self, pump: EventPump) -> Self {
        self.pump = Some(pump);
        self
    }

    pub fn with_memory_storage(mut self, config: storage::StorageConfig) -> Self {
        self.self_improvement.config = Some(config.clone());
        self.memory.config = Some(config);
        for (id, kind) in [
            ("memory-main", PanelKind::Memory),
            // A GUI-local companion tab: PanelKind belongs to workspace-ui, so
            // preserve that schema and dispatch by this distinct ID in the viewer.
            ("self-improvement-main", PanelKind::Memory),
            ("tasks-main", PanelKind::Tasks),
        ] {
            let id = PanelId::new(id);
            if self.dock.find_tab(&id).is_none() {
                self.panels.insert(
                    id.clone(),
                    Panel {
                        id: id.clone(),
                        kind,
                        title: if id.as_str() == "self-improvement-main" {
                            "Self-improvement drafts".into()
                        } else {
                            kind.default_title().into()
                        },
                        target: None,
                    },
                );
                if let Some(path) = self.dock.find_tab(&PanelId::new("tasks-main"))
                    && let Ok(leaf) = self.dock.leaf_mut(path.node_path())
                {
                    leaf.tabs.push(id);
                } else {
                    self.dock.push_to_focused_leaf(id);
                }
            }
        }
        self
    }

    /// Installs startup-only visibility and the single writer used for review actions.
    /// The resolved directory is shared with runtime policy; saving settings does not
    /// change this session's collector or file location.
    pub fn with_self_improvement(
        mut self,
        handle: storage::StorageHandle,
        enabled: bool,
        draft_dir: Option<PathBuf>,
    ) -> Self {
        self.self_improvement.handle = enabled.then_some(handle);
        self.self_improvement.enabled = enabled;
        self.self_improvement.draft_dir = draft_dir;
        self
    }

    pub fn with_diagnostic_storage(
        mut self,
        handle: storage::StorageHandle,
        config: &storage::StorageConfig,
    ) -> Self {
        self.storage_settings.configure(handle, config);
        self
    }

    /// Terminal ペインがプロジェクトごとのシェルを起動するための spawner を設定します。
    pub fn with_terminal_spawner(mut self, spawner: Arc<dyn crate::pty::TerminalSpawner>) -> Self {
        self.terminals.set_spawner(spawner);
        self
    }

    pub fn with_save_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.save_path = Some(path.into());
        self
    }

    pub fn with_sidebar(mut self, sidebar: SidebarState) -> Self {
        if let Some(thread) = sidebar
            .threads
            .iter()
            .find(|thread| Some(&thread.id) == sidebar.active_thread.as_ref())
        {
            self.composer.restore_thread_role(thread);
            self.composer.input = thread.draft_input.clone();
        }
        self.transcripts
            .select_thread(sidebar.active_thread.as_ref().map(ToString::to_string));
        for thread in &sidebar.threads {
            for run in &thread.run_ids {
                self.transcripts.bind_run(run, &thread.id.to_string());
            }
        }
        self.sidebar = sidebar;
        self.sync_subagent_thread_panes();
        self.refresh_active_thread_workspace();
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

    pub fn with_artifact_opener(
        mut self,
        opener: Arc<dyn crate::model::artifact_opener::ArtifactOpener>,
    ) -> Self {
        self.artifact_opener = Some(opener);
        self
    }

    pub fn with_diff_source(mut self, source: Arc<dyn DiffSource>) -> Self {
        self.diff_source = source;
        self
    }

    pub fn with_command_sink(mut self, sink: Box<dyn CommandSink>) -> Self {
        self.sink = sink;
        self.declared_trust.clear();
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
    pub const fn notifications(&self) -> &NotificationsModel {
        &self.notifications
    }
    pub const fn pending_approvals(&self) -> &PendingApprovalsModel {
        &self.pending_approvals
    }
    pub const fn notifications_mut(&mut self) -> &mut NotificationsModel {
        &mut self.notifications
    }
    pub const fn terminals(&self) -> &crate::terminal::TerminalSessions {
        &self.terminals
    }
    /// 選択中プロジェクトの端末セッションです (未表示なら `None`)。
    pub fn active_terminal(&self) -> Option<&crate::terminal::TerminalSession> {
        self.terminals.get(&self.sidebar.selected_project)
    }
    /// 選択中プロジェクトの端末画面へ、PTY 出力として `bytes` を流し込みます。
    pub fn feed_terminal(&mut self, bytes: &[u8]) {
        let (key, cwd) = self.terminal_target();
        self.terminals
            .session_mut(&key, &cwd)
            .emulator_mut()
            .feed(bytes);
    }
    /// 選択中プロジェクトの端末セッションのキーと起動ディレクトリです。
    pub(super) fn terminal_target(&self) -> (crate::terminal::TerminalKey, PathBuf) {
        let key = self.sidebar.selected_project.clone();
        let root = key.as_ref().and_then(|id| {
            self.sidebar
                .projects
                .iter()
                .find(|project| &project.id == id)
                .map(|project| project.repo_root.as_path())
        });
        let home = self
            .home_dir
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(std::env::temp_dir);
        (key, crate::pty::resolve_terminal_cwd(root, &home))
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
