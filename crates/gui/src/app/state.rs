use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use egui_dock::DockState;
use workspace_ui::{Panel, PanelId, SidebarState, UiSettings};

use super::WorkbenchError;
use crate::diff::{DiffModel, DiffSource, GitCliDiffSource};
use crate::dock::to_dock_state;
use crate::events::EventPump;
use crate::keymap::Keymap;
use crate::model::commands::{
    CiStatus, CommandSink, FixtureLoopAdapter, GoalFormModel, MergeApprovalModel,
    MergeApprovalView, ReviewerStatus, WorkbenchCommand,
};
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
    pub(super) focus: ConversationFocus,
    pub(super) diff: DiffModel,
    pub(super) diff_source: Arc<dyn DiffSource>,
    pub(super) goal_form: GoalFormModel,
    pub(super) merge: MergeApprovalModel,
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
        let dock = to_dock_state(&workspace)?;
        let mut state = Self {
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
            focus: ConversationFocus::Thread,
            diff: DiffModel::new(),
            diff_source: Arc::new(GitCliDiffSource),
            goal_form: GoalFormModel::default(),
            merge: MergeApprovalModel {
                view: MergeApprovalView {
                    pr: None,
                    ci: CiStatus::Unknown,
                    reviewer: ReviewerStatus::Unknown,
                    diff_summary: None,
                    resolution: None,
                },
            },
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

    pub fn with_pty(mut self, pty: PtySession) -> Self {
        self.pty = Some(pty);
        self
    }

    pub fn with_save_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.save_path = Some(path.into());
        self
    }

    pub fn with_sidebar(mut self, sidebar: SidebarState) -> Self {
        self.sidebar = sidebar;
        self
    }

    pub fn with_sidebar_path(mut self, path: PathBuf) -> Self {
        self.sidebar_path = Some(path);
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
    pub const fn merge(&self) -> &MergeApprovalModel {
        &self.merge
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
