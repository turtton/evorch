//! eframe App とフレーム駆動の WorkbenchState を実装します。

use std::collections::BTreeMap;
use std::path::PathBuf;

use egui_dock::{DockArea, DockState, TabViewer};
use workspace_ui::{KeyAction, Panel, PanelId, PanelKind, UiSettings, Workspace};

use crate::dock::{DockConvertError, from_dock_state, to_dock_state};
use crate::events::EventPump;
use crate::keymap::Keymap;
use crate::model::tasks::{AgentRunSource, TasksModel};
use crate::model::terminal::TerminalBuffer;
use crate::model::transcript::TranscriptModel;
use crate::panes::{agent::agent_pane, tasks::tasks_pane, terminal::terminal_pane};
use crate::pty::PtySession;

/// WorkbenchState 構築・運用時のエラーです。
#[derive(Debug, thiserror::Error)]
pub enum WorkbenchError {
    #[error("workspace validation failed: {0}")]
    InvalidWorkspace(#[from] workspace_ui::LayoutError),
    #[error("dock conversion failed: {0}")]
    DockConvert(#[from] DockConvertError),
    #[error("persistence failed: {0}")]
    Persist(#[from] workspace_ui::PersistError),
}

/// フレームごとにイベント・レイアウト・描画を統合する状態です。
pub struct WorkbenchState<S> {
    pump: Option<EventPump>,
    transcript: TranscriptModel,
    tasks: TasksModel<S>,
    terminal: TerminalBuffer,
    pty: Option<PtySession>,
    dock: DockState<PanelId>,
    panels: BTreeMap<PanelId, Panel>,
    keymap: Keymap,
    terminal_input: String,
    save_path: Option<PathBuf>,
}

impl<S: AgentRunSource> WorkbenchState<S> {
    /// 設定と AgentRunSource から初期状態を構築します。
    pub fn new(
        source: S,
        settings: &UiSettings,
        model_label: impl Into<String>,
    ) -> Result<Self, WorkbenchError> {
        let workspace = settings.layout.workspace.clone().unwrap_or_default();
        workspace
            .validate()
            .map_err(WorkbenchError::InvalidWorkspace)?;
        let dock = to_dock_state(&workspace)?;
        let mut state = Self {
            pump: None,
            transcript: TranscriptModel::new(),
            tasks: TasksModel::new(source, model_label),
            terminal: TerminalBuffer::new(10_000),
            pty: None,
            dock,
            panels: workspace.panels,
            keymap: Keymap::from_settings(&settings.keybinds),
            terminal_input: String::new(),
            save_path: None,
        };
        state.tasks.refresh();
        Ok(state)
    }

    /// イベントポンプを接続します。
    pub fn with_pump(mut self, pump: EventPump) -> Self {
        self.pump = Some(pump);
        self
    }

    /// PTY セッションを接続します。
    pub fn with_pty(mut self, pty: PtySession) -> Self {
        self.pty = Some(pty);
        self
    }

    /// レイアウト保存先パスを設定します。
    pub fn with_save_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.save_path = Some(path.into());
        self
    }

    /// 現在の DockState を返します（テスト検証用）。
    pub fn dock(&self) -> &DockState<PanelId> {
        &self.dock
    }

    /// 現在の DockState への可変参照を返します（テスト検証用）。
    pub fn dock_mut(&mut self) -> &mut DockState<PanelId> {
        &mut self.dock
    }

    /// トランスクリプトモデルへの参照を返します（テスト検証用）。
    pub fn transcript(&self) -> &TranscriptModel {
        &self.transcript
    }

    /// タスクモデルへの参照を返します（テスト検証用）。
    pub fn tasks(&self) -> &TasksModel<S> {
        &self.tasks
    }

    /// 端末バッファへの参照を返します（テスト検証用）。
    pub fn terminal(&self) -> &TerminalBuffer {
        &self.terminal
    }

    /// レイアウト保存先パスを返します。
    pub fn save_path(&self) -> Option<&PathBuf> {
        self.save_path.as_ref()
    }

    /// 1 フレーム分の更新と描画を行います。
    pub fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_pump();
        self.drain_pty(&ctx);
        self.handle_input(&ctx);
        self.render(ui);
    }

    fn drain_pump(&mut self) {
        if let Some(pump) = &mut self.pump {
            for event in pump.drain() {
                self.transcript.apply(&event);
                self.tasks.apply_event(&event);
            }
        }
    }

    fn drain_pty(&mut self, ctx: &egui::Context) {
        if let Some(pty) = &mut self.pty {
            let output = pty.drain_output();
            if !output.is_empty() {
                self.terminal.feed(&output);
                ctx.request_repaint();
            }
        }
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        let action = ctx.input(|input| self.keymap.action_for_input(input));
        if let Some(action) = action {
            self.dispatch(action, ctx);
        }
    }

    fn dispatch(&mut self, action: KeyAction, ctx: &egui::Context) {
        match action {
            KeyAction::FocusAgentPane => self.focus_panel("agent-main"),
            KeyAction::FocusTerminalPane => self.focus_panel("terminal-main"),
            KeyAction::FocusTasksPane => self.focus_panel("tasks-main"),
            KeyAction::SaveLayout => self.save_layout(),
            KeyAction::ResetLayout => self.reset_layout(ctx),
        }
    }

    fn focus_panel(&mut self, id: &str) {
        let panel_id = PanelId::new(id);
        if let Some(tab_path) = self.dock.find_tab(&panel_id) {
            let _ = self.dock.set_active_tab(tab_path);
        }
    }

    fn save_layout(&mut self) {
        let Some(path) = self.save_path.clone() else {
            return;
        };
        match from_dock_state(&self.dock, &self.panels) {
            Ok(workspace) => {
                if let Err(error) = workspace_ui::save_to(&workspace, &path) {
                    tracing::warn!("failed to save layout: {error}");
                }
            }
            Err(error) => tracing::warn!("failed to extract workspace: {error}"),
        }
    }

    fn reset_layout(&mut self, ctx: &egui::Context) {
        match to_dock_state(&Workspace::default_v01()) {
            Ok(dock) => {
                self.dock = dock;
                ctx.request_repaint();
            }
            Err(error) => tracing::warn!("failed to reset layout: {error}"),
        }
    }

    fn render(&mut self, ui: &mut egui::Ui) {
        let mut viewer = WorkbenchTabViewer {
            transcript: &mut self.transcript,
            tasks: &mut self.tasks,
            terminal: &mut self.terminal,
            terminal_input: &mut self.terminal_input,
            pty: &mut self.pty,
            panels: &self.panels,
        };
        DockArea::new(&mut self.dock).show_inside(ui, &mut viewer);
    }
}

/// eframe::App 実装。WorkbenchState をラップします。
pub struct WorkbenchApp<S>(pub WorkbenchState<S>);

impl<S: AgentRunSource> eframe::App for WorkbenchApp<S> {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.0.ui(ui, frame);
    }
}

struct WorkbenchTabViewer<'a, S> {
    transcript: &'a mut TranscriptModel,
    tasks: &'a mut TasksModel<S>,
    terminal: &'a mut TerminalBuffer,
    terminal_input: &'a mut String,
    pty: &'a mut Option<PtySession>,
    panels: &'a BTreeMap<PanelId, Panel>,
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

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        let Some(panel) = self.panels.get(tab) else {
            return;
        };
        match panel.kind {
            PanelKind::Agent => agent_pane(ui, self.transcript),
            PanelKind::Terminal => terminal_pane(ui, self.terminal, self.terminal_input, self.pty),
            PanelKind::Tasks => tasks_pane(ui, self.tasks),
        }
    }
}
