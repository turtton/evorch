use crate::model::routing_settings::ModelChoices;

/// Requests a profile change without mutating the candidate; the caller validates it
/// against the destination profile before applying it. Model overrides use the current
/// profile's compatible choices (ADR 0038).
pub(super) fn candidate_picker(
    ui: &mut egui::Ui,
    candidate: &mut config::RouteCandidateConfig,
    choices: (&[String], &ModelChoices, &str),
) -> Option<String> {
    let (profile_names, models, name) = choices;
    let mut selected_profile = candidate.profile.clone();
    let label = ui.label(format!("{name} profile"));
    egui::ComboBox::from_id_salt("profile")
        .width(ui.available_width())
        .selected_text(&candidate.profile)
        .show_ui(ui, |ui| {
            for name in profile_names {
                ui.selectable_value(&mut selected_profile, name.clone(), name);
            }
        })
        .response
        .labelled_by(label.id);
    let profile_changed = selected_profile != candidate.profile;
    let label = ui.label(format!("{name} model override"));
    egui::ComboBox::from_id_salt("model")
        .width(ui.available_width())
        .selected_text(candidate.model.as_deref().unwrap_or("(profile default)"))
        .show_ui(ui, |ui| {
            if models.profile_default {
                ui.selectable_value(&mut candidate.model, None, "(profile default)");
            }
            for name in &models.models {
                ui.selectable_value(&mut candidate.model, Some(name.clone()), name);
            }
        })
        .response
        .labelled_by(label.id);
    profile_changed.then_some(selected_profile)
}
