//! eframe App とフレーム駆動の WorkbenchState を実装します。

mod actions;
mod attention;
pub mod auto_title;
mod branching;
mod composer;
mod conversation;
mod escalation;
mod external_commands;
mod frame;
mod history;
mod input;
mod mentions;
mod ownership;
mod ownership_status;
mod project_dialog;
mod provider_settings;
mod questions;
mod restoration;
mod role_profiles;
mod role_settings;
mod routing_settings;
mod sandbox_settings;
mod self_improvement_settings;
mod state;
mod storage_settings;
mod subagent_dock;
mod system_notifications;
mod tab_viewer;
mod theme_settings;
mod thread_archive;
mod usage_ledger;
mod viewer;
mod work_panels;

pub use attention::ack::{AttentionAck, DisplayRevision};
pub use state::{ConversationFocus, WorkbenchState};

use crate::dock::DockConvertError;
use crate::model::tasks::AgentRunSource;

/// WorkbenchState 構築・運用時のエラーです。
#[derive(Debug, thiserror::Error)]
pub enum WorkbenchError {
    #[error(transparent)]
    ProjectPath(#[from] crate::model::project_path::ProjectPathError),
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
    #[error("{0}")]
    Branch(&'static str),
}

/// eframe::App 実装。WorkbenchState をラップします。
pub struct WorkbenchApp<S>(pub WorkbenchState<S>);

impl<S: AgentRunSource> eframe::App for WorkbenchApp<S> {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.0.logic(ctx);
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        self.0.raw_input_hook(raw_input);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.0.ui(ui, frame);
    }
}
