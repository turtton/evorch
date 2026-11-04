//! eframe App とフレーム駆動の WorkbenchState を実装します。

mod actions;
mod agent_browser;
mod agent_features;
mod attention;
mod auto_grouping;
pub mod auto_title;
mod branching;
mod composer;
mod composer_draft;
mod conversation;
mod deferred_bodies;
#[cfg(test)]
mod deferred_bodies_tests;
mod delivery;
#[cfg(test)]
mod delivery_tests;
mod escalation;
mod evaluations;
mod external_commands;
mod external_work;
#[cfg(test)]
mod family_state_tests;
mod follow_ups;
mod frame;
mod frame_trace;
mod history;
#[cfg(test)]
mod history_restore_tests;
mod input;
mod mentions;
mod ownership;
mod ownership_status;
mod project_dialog;
mod provider_settings;
mod questions;
mod repaint_policy;
mod restoration;
mod role_profiles;
mod role_settings;
mod routing_settings;
mod sandbox_settings;
mod self_improvement_settings;
mod shared_feed;
mod shared_sidebar;
pub mod splash;
mod state;
mod storage_settings;
mod subagent_dock;
mod system_notifications;
mod tab_viewer;
mod theme_settings;
mod thread_archive;
mod thread_mirror;
mod thread_retention;
mod thread_search;
mod usage_ledger;
mod viewer;
mod work_panels;

pub use attention::ack::{AttentionAck, DisplayRevision};
pub use state::{ConversationFocus, WorkbenchState};
pub use thread_retention::{
    RetentionScheduler, RetentionSnapshot, StorageRetentionScheduler, confirmed_threads,
    prune_deleted, startup_notice,
};

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
    #[error("Could not save the sidebar (another window may be saving it). Try again: {0}")]
    SidebarBusy(String),
}

impl WorkbenchError {
    /// The shared sidebar could not be edited: the action did not happen.
    pub(crate) fn sidebar_busy(error: Box<dyn std::error::Error>) -> Self {
        match error.downcast::<workspace_ui::ThreadError>() {
            Ok(error) => Self::Thread(*error),
            Err(error) => Self::SidebarBusy(error.to_string()),
        }
    }
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

    fn on_exit(&mut self) {
        self.0.flush_composer_draft();
    }
}
