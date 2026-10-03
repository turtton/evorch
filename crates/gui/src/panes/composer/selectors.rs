use super::{ComposerAction, SandboxPickerContext};
use crate::model::composer::ComposerRole;
use crate::model::model_picker::ModelPickerState;
use crate::panes::model_picker::{ModelPickerContext, model_picker};
use crate::theme::icons;

pub(super) fn row(
    ui: &mut egui::Ui,
    sandbox: SandboxPickerContext,
    (role, role_locked): (ComposerRole, bool),
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
        let icon = match role {
            ComposerRole::Worker => icons::ROBOT,
            ComposerRole::Orchestrator => icons::TREE_STRUCTURE,
        };
        let label = format!("Role: {}", role.label());
        let response = ui
            .add_enabled(
                !role_locked,
                egui::Button::new(icons::with_icon(icon, role.label())),
            )
            .on_hover_text("送信先の role を切替")
            .on_disabled_hover_text("このスレッドで固定");
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, !role_locked, &label)
        });
        if response.clicked() {
            action = Some(ComposerAction::ToggleRole);
        }
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
        let response = ui
            .button("Storage")
            .on_hover_text("Diagnostic history and database usage");
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Storage settings")
        });
        if response.clicked() {
            action = Some(ComposerAction::OpenStorageSettings);
        }
    });
    action
}
