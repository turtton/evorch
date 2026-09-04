//! Agent list pane stub.

use crate::model::tasks::{AgentRunSource, TasksModel};
use crate::model::telemetry::TelemetryOverlay;

pub fn agents_pane<S: AgentRunSource>(
    ui: &mut egui::Ui,
    _tasks: &TasksModel<S>,
    _telemetry: &TelemetryOverlay,
) {
    ui.heading("Agents");
    ui.label("Agent runs and telemetry");
}
