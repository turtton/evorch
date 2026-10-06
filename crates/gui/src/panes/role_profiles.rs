//! Role profile controls shown at the top of the role and routing settings modals.

use crate::model::role_profiles::{RoleProfileAction, RoleProfilePicker};
use crate::theme::{text::muted, tokens::*};

pub const PROFILE_LABEL: &str = "Role profile";
pub const NEW_NAME_LABEL: &str = "New profile name";
pub const NEW_BUTTON: &str = "New profile";
pub const DELETE_BUTTON: &str = "Delete profile";

pub fn role_profile_bar(
    ui: &mut egui::Ui,
    id_salt: &str,
    picker: &mut RoleProfilePicker,
) -> Option<RoleProfileAction> {
    let mut action = None;
    ui.push_id(id_salt, |ui| {
        ui.horizontal(|ui| {
            let label = ui.label(PROFILE_LABEL);
            let mut selected = picker.selected.clone();
            egui::ComboBox::from_id_salt("role-profile")
                .selected_text(&selected)
                .show_ui(ui, |ui| {
                    for name in &picker.names {
                        ui.selectable_value(&mut selected, name.clone(), name);
                    }
                })
                .response
                .labelled_by(label.id);
            if selected != picker.selected {
                action = Some(RoleProfileAction::Select(selected));
            }
            if ui
                .add_enabled(picker.can_delete(), egui::Button::new(DELETE_BUTTON))
                .clicked()
            {
                action = Some(RoleProfileAction::Delete(picker.selected.clone()));
            }
        });
        ui.horizontal(|ui| {
            let label = ui.label(NEW_NAME_LABEL);
            ui.add(
                egui::TextEdit::singleline(&mut picker.new_name)
                    .desired_width((ui.available_width() - SP_4 * 8.0).max(SP_4 * 8.0))
                    .background_color(palette().INPUT)
                    .hint_text("lowercase-name"),
            )
            .labelled_by(label.id);
            if ui
                .button(NEW_BUTTON)
                .on_hover_text(format!(
                    "Copy the saved '{}' profile under the new name",
                    picker.selected
                ))
                .clicked()
            {
                action = Some(RoleProfileAction::Create(picker.new_name.clone()));
            }
        });
        if let Some(notice) = picker.notice() {
            let color = if picker.edits_effective() {
                palette().TEXT_MUTED
            } else {
                palette().WARNING_FG
            };
            ui.colored_label(color, notice);
        } else {
            ui.label(muted(
                "Saved to your user config; projects pick a profile in Project settings.",
            ));
        }
    });
    action
}
