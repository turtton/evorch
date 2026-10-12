pub(super) fn candidate_picker(
    ui: &mut egui::Ui,
    candidate: &mut config::RouteCandidateConfig,
    choices: (
        &[String],
        &std::collections::BTreeMap<String, Vec<String>>,
        &str,
    ),
) {
    let previous_profile = candidate.profile.clone();
    let label = ui.label(format!("{} profile", choices.2));
    egui::ComboBox::from_id_salt("profile")
        .width(ui.available_width())
        .selected_text(&candidate.profile)
        .show_ui(ui, |ui| {
            for name in choices.0 {
                ui.selectable_value(&mut candidate.profile, name.clone(), name);
            }
        })
        .response
        .labelled_by(label.id);
    if candidate.profile != previous_profile
        && let Some(model) = &candidate.model
        && !choices
            .1
            .get(&candidate.profile)
            .is_some_and(|models| models.contains(model))
    {
        candidate.model = None;
    }
    let label = ui.label(format!("{} model override", choices.2));
    egui::ComboBox::from_id_salt("model")
        .width(ui.available_width())
        .selected_text(candidate.model.as_deref().unwrap_or("(profile default)"))
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut candidate.model, None, "(profile default)");
            if let Some(models) = choices.1.get(&candidate.profile) {
                for name in models {
                    ui.selectable_value(&mut candidate.model, Some(name.clone()), name);
                }
            }
        })
        .response
        .labelled_by(label.id);
}
