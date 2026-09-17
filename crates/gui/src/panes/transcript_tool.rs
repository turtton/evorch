use std::borrow::Cow;

use egui::{Color32, RichText, Ui};

use crate::model::transcript::{ToolStatus, TranscriptEntry};
use crate::theme::tokens::{FONT_SMALL, R_SM, SP_2, palette};
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
    let running = matches!(status, ToolStatus::Running);
    let mut expanded = ui.data(|data| data.get_temp::<bool>(id).unwrap_or(false));
    let status_color = match status {
        ToolStatus::Running => palette().INFO,
        ToolStatus::Succeeded => palette().SUCCESS,
        ToolStatus::Failed => palette().ERROR_FG,
        ToolStatus::AwaitingApproval => palette().WARNING_FG,
        ToolStatus::Approved => palette().SUCCESS,
        ToolStatus::Denied { .. } => palette().ERROR_FG,
    };
    let color = if *is_error {
        palette().ERROR_FG
    } else {
        status_color
    };
    let short_id: String = call_id.chars().take(8).collect();
    surface_frame(palette().SURFACE).show(ui, |ui| {
        let arrow = if running {
            ""
        } else if expanded {
            "v"
        } else {
            ">"
        };
        let header = format!("{arrow} {tool_name} ({short_id})");
        let response = ui.horizontal(|ui| {
            if running {
                ui.add(
                    egui::Spinner::new()
                        .size(FONT_SMALL)
                        .color(palette().RUNNING),
                );
            }
            ui.add_enabled(
                !running,
                egui::Button::new(RichText::new(header).color(color))
                    .frame(false)
                    .wrap(),
            )
            .on_hover_text(call_id)
        });
        if running {
            return;
        }
        if response.inner.clicked() {
            expanded = !expanded;
            ui.data_mut(|data| data.insert_temp(id, expanded));
        }
        if expanded {
            if let Some(input) = input {
                ui.label("Input");
                let content = focused_input(tool_name, input)
                    .map(Cow::Borrowed)
                    .unwrap_or_else(|| Cow::Owned(pretty_json(input)));
                code(ui, &content, palette().TEXT);
            }
            if let Some(output) = output {
                ui.label(if *is_error { "Error" } else { "Output" });
                code(
                    ui,
                    &display_output(tool_name, output),
                    if *is_error {
                        palette().ERROR_FG
                    } else {
                        palette().TEXT
                    },
                );
            }
            if let Some(detail) = detail {
                ui.label("Detail");
                code(
                    ui,
                    &pretty_json(detail),
                    if *is_error {
                        palette().ERROR_FG
                    } else {
                        palette().TEXT
                    },
                );
            }
            if let ToolStatus::Denied { reason } = status {
                code(ui, reason, palette().ERROR_FG);
            }
        } else if let Some(output) = output {
            let output = display_output(tool_name, output);
            let preview = output.lines().take(5).collect::<Vec<_>>().join("\n");
            if !preview.is_empty() {
                code(
                    ui,
                    &preview,
                    if *is_error {
                        palette().ERROR_FG
                    } else {
                        palette().TEXT
                    },
                );
            }
            if output.lines().nth(5).is_some() {
                ui.label("... expand for full output");
            }
        }
    });
}

fn focused_input<'a>(tool_name: &str, input: &'a serde_json::Value) -> Option<&'a str> {
    match tool_name {
        "bash" | "shell" => input.get("command").and_then(serde_json::Value::as_str),
        "read" | "write" | "edit" => ["file_path", "path", "filePath", "file"]
            .iter()
            .find_map(|key| input.get(key).and_then(serde_json::Value::as_str)),
        _ => None,
    }
}

fn display_output<'a>(tool_name: &str, output: &'a str) -> Cow<'a, str> {
    if matches!(tool_name, "bash" | "shell")
        && let Some((exit, streams)) = output.split_once("\n--- stdout ---\n")
        && let Some(code) = exit.strip_prefix("exit_code: ")
        && code.parse::<i32>().is_ok()
        && let Some((stdout, stderr)) = streams.split_once("\n--- stderr ---\n")
    {
        let mut combined = exit.to_owned();
        for stream in [stdout, stderr] {
            if !stream.is_empty() {
                if !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str(stream);
            }
        }
        return Cow::Owned(combined);
    }
    Cow::Borrowed(output)
}

fn pretty_json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|error| error.to_string())
}

fn code(ui: &mut Ui, content: &str, color: Color32) {
    egui::Frame::new()
        .fill(palette().SURFACE_RAISED)
        .corner_radius(R_SM)
        .inner_margin(SP_2)
        .show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(content).monospace().color(color)).wrap());
        });
}
