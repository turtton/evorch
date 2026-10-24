use crate::model::routing_settings::ModelChoices;

/// Edits one route candidate. The model override offers only `models`, the models whose
/// kind matches the route's other candidates (ADR 0038). Returns whether the profile
/// changed, so the caller can refit the model override to the new profile.
pub(super) fn candidate_picker(
    ui: &mut egui::Ui,
    candidate: &mut config::RouteCandidateConfig,
    choices: (&[String], &ModelChoices, &str),
) -> bool {
    let (profile_names, models, name) = choices;
    let previous_profile = candidate.profile.clone();
    let label = ui.label(format!("{name} profile"));
    egui::ComboBox::from_id_salt("profile")
        .width(ui.available_width())
        .selected_text(&candidate.profile)
        .show_ui(ui, |ui| {
            for name in profile_names {
                ui.selectable_value(&mut candidate.profile, name.clone(), name);
            }
        })
        .response
        .labelled_by(label.id);
    let profile_changed = candidate.profile != previous_profile;
    let label = ui.label(format!("{name} model override"));
    egui::ComboBox::from_id_salt("model")
        .width(ui.available_width())
        .selected_text(candidate.model.as_deref().unwrap_or("(profile default)"))
        .show_ui(ui, |ui| {
            if models.profile_default || candidate.model.is_none() {
                ui.selectable_value(&mut candidate.model, None, "(profile default)");
            }
            for name in &models.models {
                ui.selectable_value(&mut candidate.model, Some(name.clone()), name);
            }
        })
        .response
        .labelled_by(label.id);
    profile_changed
}
