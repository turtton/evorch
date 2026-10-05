pub(super) fn candidate_picker(
    ui: &mut egui::Ui,
    candidate: &mut config::RouteCandidateConfig,
    choices: (
        &[String],
        &std::collections::BTreeMap<String, Vec<String>>,
        &str,
    ),
    efforts: &[String],
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
    let label = ui.label(format!("{} reasoning effort", choices.2));
    effort_picker(
        ui,
        "reasoning-effort",
        "(provider default)",
        &mut candidate.reasoning_effort,
        efforts,
    )
    .labelled_by(label.id);
}

/// 推論強度の ComboBox。一覧外の現在値は `(custom)` として保持する。
fn effort_picker(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    unset: &str,
    value: &mut Option<String>,
    choices: &[String],
) -> egui::Response {
    let custom = value
        .as_ref()
        .filter(|current| !choices.contains(current))
        .cloned();
    let selected = match (value.as_deref(), &custom) {
        (None, _) => unset.to_owned(),
        (Some(current), Some(_)) => format!("{current} (custom)"),
        (Some(current), None) => current.to_owned(),
    };
    egui::ComboBox::from_id_salt(id_salt)
        .selected_text(selected)
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, unset);
            for effort in choices {
                ui.selectable_value(value, Some(effort.clone()), effort);
            }
            if let Some(current) = custom {
                ui.selectable_value(value, Some(current.clone()), format!("{current} (custom)"));
            }
        })
        .response
}
