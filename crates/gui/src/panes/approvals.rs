use egui::{RichText, Ui};

use crate::model::pending_approvals::PendingApproval;
use crate::theme::tokens::{FONT_MONO, FONT_SMALL, SP_2, palette};
use crate::theme::widgets::copy_id_button;

use super::requests::{RequestAction, request_card};

/// Always render the complete input. Commands (or a file access's paths) get a
/// literal, selectable code block (including newlines), with all other
/// arguments retained below it.
pub fn approval_card(ui: &mut Ui, item: &PendingApproval) -> Option<RequestAction> {
    let mut action = None;
    ui.push_id(("approval", &item.call_id), |ui| {
        request_card(ui, "Tool approval needed", |ui| {
            ui.add(egui::Label::new(RichText::new(&item.tool_name).strong()).wrap());
            ui.horizontal_wrapped(|ui| {
                if let Some(attempt) = item.attempt {
                    ui.label(
                        RichText::new(format!("Attempt {attempt}"))
                            .size(FONT_SMALL)
                            .color(palette().TEXT_MUTED),
                    );
                }
                if let Some(run_id) = item.run_id.as_deref() {
                    copy_id_button(ui, "Copy run ID", run_id);
                }
            });
            match item.input.as_ref() {
                Some(input) => {
                    let primary = primary_key(input);
                    if let Some(command) = input.get(primary).and_then(serde_json::Value::as_str) {
                        full_input(ui, command);
                        let mut rest = input.clone();
                        if let Some(args) = rest.as_object_mut() {
                            args.remove(primary);
                            if !args.is_empty() {
                                full_input(
                                    ui,
                                    &serde_json::to_string_pretty(&rest).expect("JSON input"),
                                );
                            }
                        }
                    } else {
                        full_input(
                            ui,
                            &serde_json::to_string_pretty(input).expect("JSON input"),
                        );
                    }
                }
                None => {
                    ui.label("No arguments");
                }
            }
            ui.horizontal_wrapped(|ui| {
                for (label, approved) in [("Approve", true), ("Reject", false)] {
                    if ui.button(label).clicked() {
                        action = Some(RequestAction::Decide {
                            call_id: item.call_id.clone(),
                            approved,
                        });
                    }
                }
            });
        });
    });
    action
}

/// File-tool escalations lead with their target path, as shell ones do with the command.
fn primary_key(input: &serde_json::Value) -> &'static str {
    if input.get("kind").and_then(serde_json::Value::as_str) == Some("file_access") {
        "path"
    } else {
        "command"
    }
}

fn full_input(ui: &mut Ui, text: &str) {
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
