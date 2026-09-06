use crate::model::tasks::{AgentRunSource, TaskRow, TasksModel};
use crate::model::telemetry::{TelemetryOverlay, TelemetryRow};
use crate::panes::agents_columns::fit_columns;
use crate::theme::text::muted;
use crate::theme::tokens::{CELL_PAD_X, DOT_SIZE, ROW_DENSE, SP_1, TEXT, agent_phase_color};
use crate::theme::widgets::{pane_root, status_dot};
use egui::{Align, Button, Label, Layout};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentsAction {
    DrillDown(String),
    ReturnToThread,
    OpenPane(String),
    OpenDefaultPanes,
}

const HEADERS: [&str; 9] = [
    "run",
    "",
    "name",
    "role",
    "phase",
    "model",
    "provider",
    "current tool",
    "tokens (in/out)",
];

pub fn agents_pane<S: AgentRunSource>(
    ui: &mut egui::Ui,
    tasks: &TasksModel<S>,
    telemetry: &TelemetryOverlay,
) -> Option<AgentsAction> {
    pane_root(ui, "Agents", |ui| {
        let mut action = ui
            .button("Open default panes")
            .clicked()
            .then_some(AgentsAction::OpenDefaultPanes);
        ui.add_space(SP_1);

        egui::ScrollArea::horizontal().show(ui, |ui| {
            let available = ui.available_width().min(ui.clip_rect().width());
            let widths = column_widths(ui, tasks, telemetry, available);

            render_header_row(ui, &widths);
            for row in tasks.rows() {
                let run_id = row.run_id.to_string();
                let row_telemetry = telemetry.row(&run_id);
                render_data_row(ui, &widths, row, row_telemetry, &mut action);
            }
        });
        action
    })
}

fn column_widths<S: AgentRunSource>(
    ui: &egui::Ui,
    tasks: &TasksModel<S>,
    telemetry: &TelemetryOverlay,
    available: f32,
) -> Vec<f32> {
    let font_id = egui::TextStyle::Body.resolve(ui.style());
    let text_width = |text: &str| {
        ui.painter()
            .layout_no_wrap(text.into(), font_id.clone(), TEXT)
            .size()
            .x
    };
    let button_pad_x = ui.spacing().button_padding.x;
    let mut natural = [0.0_f32; 9];

    for (index, heading) in HEADERS.iter().enumerate() {
        natural[index] = text_width(heading) + CELL_PAD_X;
    }
    natural[1] = text_width("Open pane") + 2.0 * button_pad_x;

    for row in tasks.rows() {
        let run_id = row.run_id.to_string();
        natural[0] = natural[0].max(text_width(&run_id) + 2.0 * button_pad_x);
        natural[2] = natural[2].max(text_width(&row.name) + CELL_PAD_X);
        natural[3] = natural[3].max(text_width(&row.role) + CELL_PAD_X);
        let status = format!("{:?}", row.status);
        natural[4] = natural[4].max(text_width(&status) + DOT_SIZE + SP_1);

        if let Some(value) = telemetry.row(&run_id) {
            natural[5] = natural[5]
                .max(text_width(value.model.as_deref().unwrap_or("unknown")) + CELL_PAD_X);
            natural[6] = natural[6]
                .max(text_width(value.provider.as_deref().unwrap_or("unknown")) + CELL_PAD_X);
            natural[7] = natural[7]
                .max(text_width(value.current_tool.as_deref().unwrap_or("unknown")) + CELL_PAD_X);
            let usage = format!("{} / {}", value.usage.input, value.usage.output);
            natural[8] = natural[8].max(text_width(&usage) + CELL_PAD_X);
        } else {
            natural[5] = natural[5].max(text_width("unknown") + CELL_PAD_X);
            natural[6] = natural[6].max(text_width("unknown") + CELL_PAD_X);
            natural[7] = natural[7].max(text_width("unknown") + CELL_PAD_X);
            natural[8] = natural[8].max(text_width("0 / 0") + CELL_PAD_X);
        }
    }

    fit_columns(&natural, SP_1, available)
}

fn render_header_row(ui: &mut egui::Ui, widths: &[f32]) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = SP_1;
        for (index, heading) in HEADERS.iter().enumerate() {
            ui.add_sized(
                [widths[index], ROW_DENSE],
                Label::new(muted(*heading).strong()).truncate(),
            );
        }
    });
}

fn render_data_row(
    ui: &mut egui::Ui,
    widths: &[f32],
    row: &TaskRow,
    row_telemetry: Option<&TelemetryRow>,
    action: &mut Option<AgentsAction>,
) {
    let run_id = row.run_id.to_string();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = SP_1;

        if ui
            .add_sized([widths[0], ROW_DENSE], Button::new(&run_id).truncate())
            .clicked()
        {
            *action = Some(AgentsAction::DrillDown(run_id.clone()));
        }
        if ui
            .add_sized([widths[1], ROW_DENSE], Button::new("Open pane").truncate())
            .clicked()
        {
            *action = Some(AgentsAction::OpenPane(run_id.clone()));
        }
        ui.add_sized(
            [widths[2], ROW_DENSE],
            Label::new(egui::RichText::new(&row.name).color(TEXT)).truncate(),
        );
        ui.add_sized(
            [widths[3], ROW_DENSE],
            Label::new(egui::RichText::new(&row.role).color(TEXT)).truncate(),
        );
        ui.allocate_ui_with_layout(
            egui::vec2(widths[4], ROW_DENSE),
            Layout::left_to_right(Align::Center),
            |ui| {
                status_dot(ui, agent_phase_color(row.status));
                ui.add(Label::new(format!("{:?}", row.status)).truncate());
            },
        );
        let model = row_telemetry
            .and_then(|value| value.model.as_deref())
            .unwrap_or("unknown");
        ui.add_sized([widths[5], ROW_DENSE], Label::new(model).truncate());
        let provider = row_telemetry
            .and_then(|value| value.provider.as_deref())
            .unwrap_or("unknown");
        ui.add_sized([widths[6], ROW_DENSE], Label::new(provider).truncate());
        let current_tool = row_telemetry
            .and_then(|value| value.current_tool.as_deref())
            .unwrap_or("unknown");
        ui.add_sized([widths[7], ROW_DENSE], Label::new(current_tool).truncate());
        let usage = row_telemetry.map(|value| value.usage).unwrap_or_default();
        ui.add_sized(
            [widths[8], ROW_DENSE],
            Label::new(format!("{} / {}", usage.input, usage.output)).truncate(),
        );
    });
}
