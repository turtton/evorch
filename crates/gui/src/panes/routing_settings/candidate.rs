use crate::theme::tokens::palette;

pub(super) fn candidate_picker(
    ui: &mut egui::Ui,
    candidate: &mut config::RouteCandidateConfig,
    choices: (
        &[String],
        &std::collections::BTreeMap<String, Vec<String>>,
        &str,
    ),
) {
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
    let previous_model = candidate.model.clone();
    let label = ui.label(format!("{} model override", choices.2));
    egui::ComboBox::from_id_salt("model")
        .width(ui.available_width())
        .selected_text(candidate.model.as_deref().unwrap_or("(profile default)"))
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut candidate.model, None, "(profile default)");
            let mut seen = std::collections::BTreeSet::new();
            for profile in choices.0 {
                if let Some(models) = choices.1.get(profile) {
                    for name in models {
                        if seen.insert(name) {
                            ui.selectable_value(&mut candidate.model, Some(name.clone()), name);
                        }
                    }
                }
            }
        })
        .response
        .labelled_by(label.id);
    let label = ui.label(format!("{} custom model ID", choices.2));
    let mut text = candidate.model.clone().unwrap_or_default();
    if ui
        .add(
            egui::TextEdit::singleline(&mut text)
                .desired_width(ui.available_width())
                .background_color(palette().INPUT)
                .hint_text("(profile default)"),
        )
        .labelled_by(label.id)
        .changed()
    {
        candidate.model = (!text.trim().is_empty()).then_some(text);
    }
    if candidate.model != previous_model
        && let Some(model) = &candidate.model
    {
        let contains = |profile: &String| {
            choices
                .1
                .get(profile)
                .is_some_and(|models| models.contains(model))
        };
        if !contains(&candidate.profile)
            && let Some(profile) = choices.0.iter().find(|profile| contains(profile))
        {
            candidate.profile.clone_from(profile);
        }
    }
}
