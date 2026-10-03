use std::borrow::Cow;
use std::path::Path;

use egui::{
    Color32, FontId, RichText, Ui,
    text::{LayoutJob, TextFormat},
};

use crate::model::transcript::{ToolStatus, TranscriptEntry};
use crate::theme::icons;
use crate::theme::text::WEIGHT_MEDIUM;
use crate::theme::tokens::{FONT_BODY, FONT_SMALL, R_SM, SP_2, palette};
use crate::theme::widgets::soft_frame;

mod header;

use header::ToolHeader;

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
    // Only states that need attention get a color; finished calls stay muted.
    let color = match status {
        _ if *is_error => palette().ERROR_FG,
        ToolStatus::Running => palette().INFO,
        ToolStatus::Succeeded | ToolStatus::Approved => palette().TEXT_MUTED,
        ToolStatus::Failed | ToolStatus::Denied { .. } => palette().ERROR_FG,
        ToolStatus::AwaitingApproval => palette().WARNING_FG,
    };
    let header = header::tool_header(
        tool_name,
        input.as_ref(),
        output.as_deref(),
        *is_error,
        repo_root,
    );
    let header_text = header.text();
    // The accessible name keeps a textual status mark; the painted header
    // only colors the icon and leaves successful calls muted.
    let (glyph, mark) = match status {
        _ if *is_error => (icons::X, "✗ "),
        ToolStatus::Running => ("", ""),
        ToolStatus::Succeeded | ToolStatus::Approved => (header.icon, "✓ "),
        ToolStatus::Failed | ToolStatus::Denied { .. } => (icons::X, "✗ "),
        ToolStatus::AwaitingApproval => (icons::QUESTION, "? "),
    };
    let label = format!("{mark}{header_text}");
    let tooltip = match focused_input(tool_name, input.as_ref()) {
        Some(full) => format!("{call_id}\n{full}"),
        None => format!("{call_id}\n{header_text}"),
    };
    soft_frame(palette().SURFACE).show(ui, |ui| {
        let response = ui.horizontal(|ui| {
            if running {
                ui.add(
                    egui::Spinner::new()
                        .size(FONT_SMALL)
                        .color(palette().RUNNING),
                );
            }
            let width = ui.available_width() - ui.spacing().button_padding.x * 2.0;
            let job = fitted_header_job(ui, glyph, color, &header, width);
            let button = ui.add_enabled(!running, egui::Button::new(job).frame(false).truncate());
            button.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, !running, &label)
            });
            button
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
                let content = focused_input(tool_name, Some(input))
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

fn focused_input<'a>(tool_name: &str, input: Option<&'a serde_json::Value>) -> Option<&'a str> {
    let input = input?;
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

/// Lays out the header, shortening the location from the left so the
/// subject (file name, command) stays visible in narrow panes.
fn fitted_header_job(
    ui: &Ui,
    glyph: &str,
    color: Color32,
    header: &ToolHeader,
    max_width: f32,
) -> LayoutJob {
    let job_with = |location: Option<&str>| header_job(glyph, color, header, location);
    let Some(location) = header.location.as_deref() else {
        return job_with(None);
    };
    let fits = |location: &str| {
        ui.fonts_mut(|fonts| fonts.layout_job(job_with(Some(location))).size().x) <= max_width
    };
    job_with(Some(&header::shorten_location(location, fits)))
}

/// `<icon> <Verb> <subject> <location> · <meta>`: the subject in body text,
/// everything else muted.
fn header_job(
    glyph: &str,
    color: Color32,
    header: &ToolHeader,
    location: Option<&str>,
) -> LayoutJob {
    let font = FontId::proportional(FONT_BODY);
    let muted = TextFormat::simple(font.clone(), palette().TEXT_MUTED);
    let mut job = LayoutJob::default();
    if !glyph.is_empty() {
        job.append(
            &format!("{glyph} "),
            0.0,
            TextFormat::simple(FontId::proportional(FONT_BODY + 1.0), color),
        );
    }
    let mut verb = muted.clone();
    verb.coords.push("wght", WEIGHT_MEDIUM);
    job.append(&header.verb, 0.0, verb);
    if !header.subject.is_empty() {
        job.append(
            &format!(" {}", header.subject),
            0.0,
            TextFormat::simple(font, palette().TEXT),
        );
    }
    if let Some(location) = location {
        job.append(&format!("  {location}"), 0.0, muted.clone());
    }
    if let Some(meta) = &header.meta {
        job.append(&format!(" · {meta}"), 0.0, muted);
    }
    job
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
