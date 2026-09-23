use std::borrow::Cow;
use std::path::Path;

use egui::{
    Color32, FontId, RichText, Ui,
    text::{LayoutJob, TextFormat},
};

use crate::model::transcript::{ToolStatus, TranscriptEntry};
use crate::theme::tokens::{FONT_SMALL, R_SM, SP_2, palette};
use crate::theme::widgets::surface_frame;

pub fn tool_card(ui: &mut Ui, entry: &TranscriptEntry, pane_id: egui::Id) {
    tool_card_with_repo_root(ui, entry, pane_id, None);
}

pub fn tool_card_with_repo_root(
    ui: &mut Ui,
    entry: &TranscriptEntry,
    pane_id: egui::Id,
    repo_root: Option<&Path>,
) {
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
    let summary = compact_summary(tool_name, input.as_ref(), repo_root)
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut summary_chars = summary.chars();
    let mut compact_summary: String = summary_chars.by_ref().take(120).collect();
    if summary_chars.next().is_some() {
        compact_summary.push('…');
    }
    let icon = match status {
        ToolStatus::Running => "",
        ToolStatus::Succeeded | ToolStatus::Approved => "✓ ",
        ToolStatus::Failed | ToolStatus::Denied { .. } => "✗ ",
        ToolStatus::AwaitingApproval => "? ",
    };
    let mut header = format!("{icon}{tool_name}");
    if !compact_summary.is_empty() {
        header.push(' ');
        header.push_str(&compact_summary);
    }
    let tooltip = if summary.is_empty() {
        call_id.clone()
    } else {
        format!("{call_id}\n{summary}")
    };
    surface_frame(palette().SURFACE).show(ui, |ui| {
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
                    .truncate(),
            )
            .on_hover_text(&tooltip)
            .on_disabled_hover_text(&tooltip)
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
                let is_diff = !*is_error && matches!(tool_name.as_str(), "edit" | "write");
                ui.label(if *is_error {
                    "Error"
                } else if is_diff {
                    "Diff"
                } else {
                    "Output"
                });
                if is_diff {
                    diff_code(ui, output);
                } else {
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

pub fn compact_summary(
    tool_name: &str,
    input: Option<&serde_json::Value>,
    repo_root: Option<&Path>,
) -> Option<String> {
    let input = input?;
    match tool_name {
        "grep" => {
            let pattern = input.get("pattern").and_then(serde_json::Value::as_str)?;
            let path = input
                .get("path")
                .and_then(serde_json::Value::as_str)
                .map(|path| {
                    repo_root
                        .and_then(|root| Path::new(path).strip_prefix(root).ok())
                        .unwrap_or_else(|| Path::new(path))
                        .display()
                        .to_string()
                });
            Some(match path {
                Some(path) => format!("{pattern} {path}"),
                None => pattern.to_owned(),
            })
        }
        _ => focused_input(tool_name, input).map(str::to_owned),
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

fn diff_code(ui: &mut Ui, content: &str) {
    let mut job = LayoutJob::default();
    for line in content.split_inclusive('\n') {
        let color = if line.starts_with('+') && !line.starts_with("+++ ") {
            palette().SUCCESS
        } else if line.starts_with('-') && !line.starts_with("--- ") {
            palette().ERROR_FG
        } else if line.starts_with("@@") {
            palette().INFO
        } else {
            palette().TEXT
        };
        job.append(
            line,
            0.0,
            TextFormat {
                font_id: FontId::monospace(FONT_SMALL),
                color,
                ..Default::default()
            },
        );
    }
    egui::Frame::new()
        .fill(palette().SURFACE_RAISED)
        .corner_radius(R_SM)
        .inner_margin(SP_2)
        .show(ui, |ui| {
            egui::ScrollArea::horizontal().show(ui, |ui| {
                ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Extend));
            });
        });
}
