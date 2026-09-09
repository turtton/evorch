use egui::{Color32, RichText, Ui};

use crate::model::transcript::{ToolStatus, TranscriptEntry};
use crate::theme::tokens::{
    ERROR_FG, INFO, R_SM, SP_2, SUCCESS, SURFACE, SURFACE_RAISED, TEXT, WARNING_FG,
};
use crate::theme::widgets::surface_frame;

pub fn tool_card(ui: &mut Ui, entry: &TranscriptEntry, pane_id: egui::Id) {
    let TranscriptEntry::Tool {
        tool_name,
        call_id,
        input,
        output,
        detail,
        is_error,
        status,
    } = entry
    else {
        return;
    };
    let id = pane_id.with(("tool-expanded", call_id));
    let mut expanded = ui.data(|data| data.get_temp::<bool>(id).unwrap_or(false));
    let (indicator, status_color) = match status {
        ToolStatus::Running => ("Running", INFO),
        ToolStatus::Succeeded => ("OK", SUCCESS),
        ToolStatus::Failed => ("ERROR", ERROR_FG),
        ToolStatus::AwaitingApproval => ("Awaiting approval", WARNING_FG),
        ToolStatus::Approved => ("Approved", SUCCESS),
        ToolStatus::Denied { .. } => ("Denied", ERROR_FG),
    };
    let color = if *is_error { ERROR_FG } else { status_color };
    let indicator = if *is_error { "ERROR" } else { indicator };
    let short_id: String = call_id.chars().take(8).collect();
    let summary = input
        .as_ref()
        .and_then(|input| input.get("file_path").or_else(|| input.get("command")))
        .and_then(serde_json::Value::as_str);
    surface_frame(SURFACE).show(ui, |ui| {
        let arrow = if expanded { "v" } else { ">" };
        let mut header = format!("{arrow} {indicator} {tool_name} ({short_id})");
        if let Some(summary) = summary {
            header.push_str(": ");
            header.extend(summary.chars().take(120));
        }
        if ui
            .add(
                egui::Button::new(RichText::new(header).color(color))
                    .frame(false)
                    .wrap(),
            )
            .on_hover_text(call_id)
            .clicked()
        {
            expanded = !expanded;
            ui.data_mut(|data| data.insert_temp(id, expanded));
        }
        if matches!(status, ToolStatus::Running) {
            ui.spinner();
        }
        if expanded {
            if let Some(input) = input {
                ui.label("Input");
                code(ui, &pretty_json(input), TEXT);
            }
            if let Some(output) = output {
                ui.label(if *is_error { "Error" } else { "Output" });
                code(ui, output, if *is_error { ERROR_FG } else { TEXT });
            }
            if let Some(detail) = detail {
                ui.label("Detail");
                code(
                    ui,
                    &pretty_json(detail),
                    if *is_error { ERROR_FG } else { TEXT },
                );
            }
            if let ToolStatus::Denied { reason } = status {
                code(ui, reason, ERROR_FG);
            }
        }
    });
}

fn pretty_json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|error| error.to_string())
}

fn code(ui: &mut Ui, content: &str, color: Color32) {
    egui::Frame::new()
        .fill(SURFACE_RAISED)
        .corner_radius(R_SM)
        .inner_margin(SP_2)
        .show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(content).monospace().color(color)).wrap());
        });
}
