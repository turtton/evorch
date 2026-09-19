use super::{ComposerAction, SandboxPickerContext};
use crate::model::model_picker::ModelPickerState;
use crate::panes::model_picker::{ModelPickerContext, model_picker};

pub(super) fn row(
    ui: &mut egui::Ui,
    sandbox: SandboxPickerContext,
    picker: (ModelPickerContext<'_>, &mut ModelPickerState),
) -> Option<ComposerAction> {
    let mut action = None;
    ui.horizontal_top(|ui| {
        ui.add_enabled_ui(sandbox.enabled, |ui| {
            let label = match sandbox.mode {
                config::EscalationApproval::Quick => "Sandbox: quick",
                config::EscalationApproval::User => "Sandbox: user",
                config::EscalationApproval::Off => "Sandbox: off",
            };
            let response = egui::ComboBox::from_id_salt("sandbox-escalation")
                .selected_text(label)
                .show_ui(ui, |ui| {
                    for (mode, label) in [
                        (config::EscalationApproval::Quick, "quick"),
                        (config::EscalationApproval::User, "user"),
                        (config::EscalationApproval::Off, "off"),
                    ] {
                        if ui
                            .selectable_label(
                                sandbox.mode == mode,
                                egui::RichText::new(label)
                                    .color(crate::theme::tokens::palette().TEXT),
                            )
                            .clicked()
                            && sandbox.mode != mode
                        {
                            action = Some(ComposerAction::SandboxEscalation(mode));
                        }
                    }
                });
            response.response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, sandbox.enabled, label)
            });
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            if let Some(preference) = model_picker(ui, picker.0, picker.1) {
                action = Some(ComposerAction::ModelPreference(preference));
            }
        });
    });
    action
}
