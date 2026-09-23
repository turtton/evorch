use egui::{RichText, Ui};

use crate::model::{pending_approvals::PendingApproval, scoped_call::parse_scoped_call_id};
use crate::theme::tokens::{FONT_MONO, SP_2, palette};

use super::requests::{RequestAction, request_card};

/// Always render the complete input. Commands get a literal, selectable code
/// block (including newlines), with all other arguments retained below it.
pub fn approval_card(ui: &mut Ui, item: &PendingApproval) -> Option<RequestAction> {
    let mut action = None;
    ui.push_id(("approval", &item.call_id), |ui| {
        request_card(ui, "コマンドの承認待ち", |ui| {
            ui.add(egui::Label::new(RichText::new(&item.tool_name).strong()).wrap());
            let original = parse_scoped_call_id(&item.call_id)
                .map(|(_, call, _)| call)
                .unwrap_or_else(|| item.call_id.clone());
            let call = if original.is_empty() {
                "—"
            } else {
                &original
            };
            let attempt = item.attempt.map_or_else(|| "—".into(), |n| n.to_string());
            ui.add(
                egui::Label::new(
                    RichText::new(format!(
                        "{} · {call} · attempt {attempt}",
                        item.run_id.as_deref().unwrap_or("—")
                    ))
                    .monospace()
                    .size(FONT_MONO)
                    .color(palette().TEXT_MUTED),
                )
                .wrap(),
            )
            .on_hover_text(&item.call_id);
            match item.input.as_ref() {
                Some(input) => {
                    if let Some(command) = input.get("command").and_then(serde_json::Value::as_str)
                    {
                        full_input(ui, command);
                        let mut rest = input.clone();
                        if let Some(args) = rest.as_object_mut() {
                            args.remove("command");
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
                    ui.label("引数情報なし");
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
