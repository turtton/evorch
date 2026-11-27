//! Transcript pieces drawn from plain text, shared by the embedded workbench and the
//! core client: neither the event bus nor the core's protocol reaches them.
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, RichText, Ui};

use crate::theme::icons::{self, with_icon};
use crate::theme::text::medium;
use crate::theme::tokens::*;
use crate::theme::widgets::soft_frame;

pub fn user_bubble(ui: &mut egui::Ui, text: &str, skills: &[String]) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
        let max_width = (ui.available_width() * 0.8).max(120.0);
        soft_frame(palette().SURFACE_RAISED)
            .corner_radius(egui::CornerRadius::same(R_XL))
            .show(ui, |ui| {
                ui.set_max_width(max_width);
                ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                    let response = ui.add(
                        egui::Label::new(egui::RichText::new(text).color(palette().TEXT)).wrap(),
                    );
                    let label = format!("You: {text}");
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label)
                    });
                    if !skills.is_empty() {
                        ui.horizontal_wrapped(|ui| {
                            for skill in skills {
                                ui.label(
                                    egui::RichText::new(icons::with_icon(
                                        icons::SPARKLE,
                                        format!("skill: {skill}"),
                                    ))
                                    .small()
                                    .color(palette().TEXT_MUTED),
                                )
                                .on_hover_text("Skill body attached when sent");
                            }
                        });
                    }
                });
            });
    });
}

pub fn event_line(ui: &mut egui::Ui, icon: &str, text: &str, color: Color32, hover: Option<&str>) {
    ui.horizontal_wrapped(|ui| {
        ui.add_space(SP_1);
        ui.spacing_mut().item_spacing.x = SP_2;
        ui.label(
            egui::RichText::new(icon)
                .size(FONT_SMALL + 1.0)
                .color(color),
        );
        let response = ui.add(
            egui::Label::new(
                egui::RichText::new(text)
                    .size(FONT_SMALL)
                    .color(palette().TEXT_MUTED),
            )
            .wrap(),
        );
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
        if let Some(hover) = hover {
            response.on_hover_text(hover);
        }
    });
}

pub fn role_label(ui: &mut egui::Ui, role: &str) {
    let response = ui.label(
        medium(with_icon(icons::ROBOT, role))
            .size(FONT_SMALL)
            .color(palette().TEXT_MUTED),
    );
    let label = format!("[{role}]");
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label));
}

pub fn request_card(ui: &mut Ui, title: &str, body: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(palette().SURFACE)
        .stroke(egui::Stroke::new(STATUS_STROKE, attention_color()))
        .corner_radius(R_SM)
        .inner_margin(SP_2)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new(RichText::new(title).strong().color(attention_color())).wrap());
            ui.add_space(SP_2);
            body(ui);
        });
    ui.add_space(SP_2);
}

pub fn full_input(ui: &mut Ui, text: &str) {
    egui::Frame::new()
        .fill(palette().CANVAS)
        .inner_margin(SP_2)
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(RichText::new(text).monospace().size(FONT_MONO))
                    .wrap()
                    .selectable(true),
            );
        });
    ui.add_space(SP_2);
}

pub fn code_block(ui: &mut Ui, content: &str, color: Color32) {
    egui::Frame::new()
        .fill(palette().SURFACE_RAISED)
        .corner_radius(R_SM)
        .inner_margin(SP_2)
        .show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(content).monospace().color(color)).wrap());
        });
}

pub fn diff_block(ui: &mut Ui, content: &str) {
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

/// Tracks IME preedit across frames so Enter can confirm a conversion without sending.
pub fn ime_composing(ui: &egui::Ui, id: egui::Id) -> bool {
    let mut composing = ui.data(|data| data.get_temp::<bool>(id).unwrap_or_default());
    ui.input(|input| {
        for event in &input.events {
            if let egui::Event::Ime(event) = event {
                #[allow(deprecated)] // Accept legacy backend Enabled/Disabled events too.
                match event {
                    egui::ImeEvent::Preedit { text, .. } => composing = !text.is_empty(),
                    egui::ImeEvent::Commit(_) | egui::ImeEvent::Disabled => composing = false,
                    egui::ImeEvent::Enabled | egui::ImeEvent::DeleteSurrounding { .. } => {}
                }
            }
        }
    });
    composing
}

/// The heading of one run's transcript; the run ID itself stays behind a copy button.
pub fn run_transcript_title(ui: &mut Ui, run_id: &str, agent_name: Option<&str>) {
    run_transcript_header(ui, run_id, agent_name, None, |_| {});
}

/// The heading with `status` on its row and, under it in small type, the model and
/// provider of the run when known.
pub fn run_transcript_header(
    ui: &mut Ui,
    run_id: &str,
    agent_name: Option<&str>,
    model: Option<&str>,
    status: impl FnOnce(&mut Ui),
) {
    ui.horizontal(|ui| {
        ui.label(crate::theme::text::h3(match agent_name {
            Some(name) => format!("Transcript: {name}"),
            None => "Transcript".to_owned(),
        }));
        crate::theme::widgets::copy_id_button(ui, "Copy run ID", run_id);
        status(ui);
    });
    if let Some(model) = model {
        ui.label(crate::theme::text::muted(model_line(model)));
    }
}

/// `profile/model` as "model · provider"; a model without a profile is shown as it is.
/// The profile is the part before the first slash, as a model id may hold slashes.
pub fn model_line(model: &str) -> String {
    match model.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
            format!("{model} · {provider}")
        }
        _ => model.to_owned(),
    }
}

pub fn run_transcript_empty(ui: &mut Ui, agent_name: Option<&str>) {
    crate::theme::widgets::empty_state(
        ui,
        &match agent_name {
            Some(name) => format!("No events for {name} yet"),
            None => "No events yet".to_owned(),
        },
        "Messages and tool activity will appear as this agent runs.",
        None,
    );
}

/// Stop halts only the current run; once it is stopped with subagents still
/// running, the same button becomes Stop subagents and halts the whole subtree.
pub fn stop_button(ui: &mut Ui, stopped: bool, running_children: usize) -> egui::Response {
    let (label, color, fill, hover) = if stopped {
        (
            "Stop subagents",
            palette().WARNING_FG,
            palette().SURFACE_RAISED,
            format!(
                "Stop {} (Esc)",
                if running_children == 1 {
                    "the running subagent".to_owned()
                } else {
                    format!("all {running_children} running subagents")
                }
            ),
        )
    } else {
        (
            "Stop",
            palette().ERROR_FG,
            palette().ERROR_SURFACE,
            "Stop the run (Esc). Its subagents keep running until stopped separately.".to_owned(),
        )
    };
    ui.add(egui::Button::new(RichText::new(label).color(color)).fill(fill))
        .on_hover_text(hover)
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable as _;

    // Given: models with and without a provider profile
    // When: they are shown / Then: the model leads and the profile follows, or the model alone
    #[test]
    fn model_line_names_model_then_provider() {
        assert_eq!(model_line("work/gpt-5"), "gpt-5 · work");
        assert_eq!(model_line("or/anthropic/claude"), "anthropic/claude · or");
        assert_eq!(model_line("fixed-model"), "fixed-model");
        assert_eq!(model_line("/x"), "/x");
    }

    // Given: a run header with a status and a model
    // When: it is drawn / Then: the status shares the title's row and the model is a line below
    #[test]
    fn the_status_shares_the_title_row_and_the_model_follows_below() {
        let mut harness = egui_kittest::Harness::new_ui(|ui| {
            crate::theme::install(ui.ctx());
            run_transcript_header(ui, "run-a", Some("tester"), Some("work/gpt-5"), |ui| {
                ui.label("status");
            });
        });
        harness.run();
        let title = harness.get_by_label("Transcript: tester").rect();
        let status = harness.get_by_label("status").rect();
        let model = harness.get_by_label("gpt-5 · work").rect();
        assert!(
            (title.center().y - status.center().y).abs() < title.height(),
            "status on the title row: {title:?} {status:?}"
        );
        assert!(model.top() >= title.bottom() - 1.0, "model below the title");
    }
}
