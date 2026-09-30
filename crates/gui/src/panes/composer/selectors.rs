use super::{ComposerAction, SandboxPickerContext};
use crate::model::model_picker::ModelPickerState;
use crate::panes::model_picker::{ModelPickerContext, model_picker};

pub(super) fn row(
    ui: &mut egui::Ui,
    sandbox: SandboxPickerContext,
    picker: (ModelPickerContext<'_>, &mut ModelPickerState),
) -> Option<ComposerAction> {
    let mut action = None;
    ui.horizontal_wrapped(|ui| {
        ui.add_enabled_ui(sandbox.enabled, |ui| {
            let label = match sandbox.mode {
                config::EscalationApproval::Auto => "Sandbox: auto",
                config::EscalationApproval::User => "Sandbox: user",
                config::EscalationApproval::Off => "Sandbox: off",
            };
            let response = ui.button(label);
            response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, sandbox.enabled, label)
            });
            if response.clicked() {
                action = Some(ComposerAction::OpenSandboxSettings);
            }
        });
        if let Some(preference) = model_picker(ui, picker.0, picker.1) {
            action = Some(ComposerAction::ModelPreference(preference));
        }
        // Keep the composer usable at narrow widths; expose the full settings
        // name to accessibility and in the tooltip, not as a wide inline button.
        let response = ui
            .button("Drafts")
            .on_hover_text("Self-improvement settings (default off)");
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Self-improvement settings")
        });
        if response.clicked() {
            action = Some(ComposerAction::OpenSelfImprovementSettings);
        }
    });
    action
}
