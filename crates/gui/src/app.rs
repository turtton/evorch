//! eframe App とフレーム駆動の WorkbenchState を実装します。

mod actions;
mod attention;
mod frame;
mod state;
mod tab_viewer;
mod viewer;

pub use state::{ConversationFocus, WorkbenchState};

use crate::dock::DockConvertError;
use crate::model::tasks::AgentRunSource;

/// WorkbenchState 構築・運用時のエラーです。
#[derive(Debug, thiserror::Error)]
pub enum WorkbenchError {
    #[error("workspace validation failed: {0}")]
    InvalidWorkspace(#[from] workspace_ui::LayoutError),
    #[error("dock conversion failed: {0}")]
    DockConvert(#[from] DockConvertError),
    #[error("persistence failed: {0}")]
    Persist(#[from] workspace_ui::PersistError),
    #[error("project state failed: {0}")]
    Project(#[from] workspace_ui::ProjectError),
    #[error("thread state failed: {0}")]
    Thread(#[from] workspace_ui::ThreadError),
}

/// eframe::App 実装。WorkbenchState をラップします。
pub struct WorkbenchApp<S>(pub WorkbenchState<S>);

impl<S: AgentRunSource> eframe::App for WorkbenchApp<S> {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.0.ui(ui, frame);
    }
}
