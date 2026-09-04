use crate::model::tasks::{AgentRunSource, TasksModel};
use crate::model::telemetry::TelemetryOverlay;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentsAction {
    DrillDown(String),
    ReturnToThread,
    OpenPane(String),
    OpenDefaultPanes,
}

pub fn agents_pane<S: AgentRunSource>(
    ui: &mut egui::Ui,
    tasks: &TasksModel<S>,
    telemetry: &TelemetryOverlay,
) -> Option<AgentsAction> {
    ui.heading("Agents");
    let mut action = ui
        .button("Open default panes")
        .clicked()
        .then_some(AgentsAction::OpenDefaultPanes);
    ui.separator();
    egui::Grid::new("agents-telemetry-table")
        .striped(true)
        .show(ui, |ui| {
            for heading in [
                "run",
                "",
                "name",
                "role",
                "phase",
                "model",
                "provider",
                "current tool",
                "tokens (in/out)",
            ] {
                ui.strong(heading);
            }
            ui.end_row();

            for row in tasks.rows() {
                let run_id = row.run_id.to_string();
                let telemetry = telemetry.row(&run_id);
                if ui.button(&run_id).clicked() {
                    action = Some(AgentsAction::DrillDown(run_id.clone()));
                }
                if ui.button("Open pane").clicked() {
                    action = Some(AgentsAction::OpenPane(run_id.clone()));
                }
                ui.label(&row.name);
                ui.label(&row.role);
                ui.label(format!("{:?}", row.status));
                ui.label(
                    telemetry
                        .and_then(|value| value.model.as_deref())
                        .unwrap_or("unknown"),
                );
                ui.label(
                    telemetry
                        .and_then(|value| value.provider.as_deref())
                        .unwrap_or("unknown"),
                );
                ui.label(
                    telemetry
                        .and_then(|value| value.current_tool.as_deref())
                        .unwrap_or("unknown"),
                );
                let usage = telemetry.map(|value| value.usage).unwrap_or_default();
                ui.label(format!("{} / {}", usage.input, usage.output));
                ui.end_row();
            }
        });
    action
}
