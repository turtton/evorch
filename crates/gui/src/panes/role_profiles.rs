//! Role profile controls shown at the top of the role and routing settings modals.

use crate::model::role_profiles::{RoleProfileAction, RoleProfilePicker};
use crate::theme::{
    icons,
    text::{muted, section},
    tokens::*,
    widgets::accessible,
};

pub const PROFILE_LABEL: &str = "Role profile";
pub const NEW_BUTTON: &str = "New profile";
pub const DELETE_BUTTON: &str = "Delete profile";
pub const PROFILE_LOCKED_NOTICE: &str = "Profile changes are disabled while edits are unsaved. Save them, or close settings to discard.";

/// The profile control of the Model Settings header: the profile in use and a `…` menu that
/// creates a profile from it or deletes it. `active` is false where the profile does not apply
/// (Providers), which draws it muted; `locked` (edits are unsaved) disables changes and says why.
pub fn role_profile_header(
    ui: &mut egui::Ui,
    picker: &mut RoleProfilePicker,
    active: bool,
    locked: bool,
) -> Option<RoleProfileAction> {
    let mut action = None;
    let enabled = active && !locked;
    // Laid out right to left by the header: the menu first, then the combo, then the label.
    ui.add_enabled_ui(enabled, |ui| {
        let menu = ui.menu_button(icons::DOTS_THREE, |ui| {
            ui.set_min_width(240.0);
            ui.label(section(format!("Profile: {}", picker.selected)));
            let label = ui.label("New profile name");
            ui.add(
                egui::TextEdit::singleline(&mut picker.new_name)
                    .desired_width(ui.available_width())
                    .background_color(palette().INPUT)
                    .hint_text("lowercase-name"),
            )
            .labelled_by(label.id);
            let create = ui.button(icons::with_icon(
                icons::COPY,
                format!("New profile from \"{}\"", picker.selected),
            ));
            accessible(&create, NEW_BUTTON);
            if create
                .on_hover_text(format!(
                    "Copy the saved '{}' profile under the new name",
                    picker.selected
                ))
                .clicked()
            {
                action = Some(RoleProfileAction::Create(picker.new_name.clone()));
                ui.close();
            }
            let delete = ui.add_enabled(
                picker.can_delete(),
                egui::Button::new(icons::with_icon(icons::TRASH, DELETE_BUTTON)),
            );
            accessible(&delete, DELETE_BUTTON);
            if delete
                .on_disabled_hover_text("The default profile cannot be deleted")
                .clicked()
            {
                action = Some(RoleProfileAction::Delete(picker.selected.clone()));
                ui.close();
            }
            ui.separator();
            ui.label(muted(picker.notice().unwrap_or_else(|| {
                "Saved to your user config; projects pick a profile in Project settings.".into()
            })));
        });
        accessible(&menu.response, "Profile menu");
        menu.response.on_hover_text("Profile menu");
        let mut selected = picker.selected.clone();
        let combo = egui::ComboBox::from_id_salt("model-settings-profile")
            .selected_text(&selected)
            .show_ui(ui, |ui| {
                for name in &picker.names {
                    ui.selectable_value(&mut selected, name.clone(), name);
                }
            });
        if selected != picker.selected {
            action = Some(RoleProfileAction::Select(selected));
        }
        accessible_combo(&combo.response, PROFILE_LABEL);
    });
    let label = ui.label(muted("Profile"));
    let hint = if !active {
        "Profiles apply to Routing and Agent Roles"
    } else if locked {
        PROFILE_LOCKED_NOTICE
    } else {
        "The role profile Routing and Agent Roles edit"
    };
    label.on_hover_text(hint);
    action
}

fn accessible_combo(response: &egui::Response, label: &str) {
    let enabled = response.enabled();
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, enabled, label));
}
