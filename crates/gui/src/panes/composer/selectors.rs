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
            let (mode, explanation) = sandbox_mode(sandbox.mode, sandbox.escalate_to_user_on_deny);
            let label = format!("Sandbox: {mode}");
            // The shield stands in for the "Sandbox:" prefix to keep the row narrow.
            let response = ui
                .button(icons::with_icon(icons::SHIELD, mode))
                .on_hover_text(format!("{explanation}\nClick to change sandbox settings."));
            response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, sandbox.enabled, &label)
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
            .on_hover_text("Switch the role that receives your message")
            .on_disabled_hover_text("Locked for this thread");
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, !role_locked, &label)
        });
        if response.clicked() {
            action = Some(ComposerAction::ToggleRole);
        }
        if let Some(preference) = model_picker(ui, picker.0, picker.1) {
            action = Some(ComposerAction::ModelPreference(preference));
        }
    });
    action
}

/// Short name and explanation of a `sandbox.escalation_approval` mode, which
/// decides requests to run shell commands or access files outside the sandbox.
pub(crate) fn sandbox_mode(
    mode: config::EscalationApproval,
    escalate_to_user_on_deny: bool,
) -> (&'static str, &'static str) {
    match mode {
        config::EscalationApproval::Auto if escalate_to_user_on_deny => (
            "auto review",
            "A reviewer model approves or denies requests to run commands or access files outside the sandbox. Requests it denies come to you for approval.",
        ),
        config::EscalationApproval::Auto => (
            "auto review",
            "A reviewer model approves or denies requests to run commands or access files outside the sandbox. Requests it denies are rejected.",
        ),
        config::EscalationApproval::User => (
            "ask me",
            "You approve or deny each request to run commands or access files outside the sandbox.",
        ),
        config::EscalationApproval::Off => (
            "always deny",
            "Requests to run commands or access files outside the sandbox are always denied. The sandbox itself stays on.",
        ),
        config::EscalationApproval::Yolo => (
            "yolo",
            "Dangerous: always allow execution and file access outside the sandbox without review or asking you. The sandbox still applies to everything else.",
        ),
    }
}
