use crate::model::routing_settings::RoutingSettingsModel;
use crate::theme::{
    text::{h3, muted},
    tokens::*,
    widgets::{primary_button, surface_frame},
};

pub enum RoutingSettingsAction {
    Save,
    Cancel,
}

pub fn routing_settings_modal(
    ctx: &egui::Context,
    model: &mut RoutingSettingsModel,
) -> Option<RoutingSettingsAction> {
    let mut action = None;
    let busy = model.is_saving();
    egui::Modal::new(egui::Id::new("routing-settings"))
.backdrop_color(palette().OVERLAY)
.frame(surface_frame(palette().SURFACE_RAISED))
        .show(ctx, |ui| {
            ui.set_width((ctx.viewport_rect().width() * 0.6).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 4.0);
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            ui.label(h3("Routing settings"));
            ui.label(muted("Candidates are tried from top to bottom. An empty override uses the profile default."));
            ui.add_enabled_ui(!busy, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("routing-routes")
                    .max_height((ctx.viewport_rect().height() - TOPBAR * 5.0).max(TOPBAR))
                    .show(ui, |ui| route_list(ui, model));
            });
            if let Some(error) = &model.validation_error {
ui.colored_label(palette().ERROR_FG, error);
            }
            if busy {
                ui.label(muted("Saving and reloading runtime..."));
            }
            ui.add_enabled_ui(!busy, |ui| {
                ui.horizontal(|ui| {
                    if primary_button(ui, "Save routing").clicked() {
                        action = Some(RoutingSettingsAction::Save);
                    }
                    if ui.button("Cancel").clicked() {
                        action = Some(RoutingSettingsAction::Cancel);
                    }
                });
            });
        });
    action
}

fn route_list(ui: &mut egui::Ui, model: &mut RoutingSettingsModel) {
    if model.routes_empty {
        surface_frame(palette().WARNING_SURFACE).show(ui, |ui| {
            let message = model
                .profile_names
                .first()
                .and_then(|name| {
                    model.profile_defaults.get(name).map(|default| format!(
                    "No explicit routes: all logical models implicitly resolve to {name}/{default}."
                ))
                })
                .unwrap_or_else(|| {
                    "No explicit routes and no provider profile: logical models cannot resolve."
                        .into()
                });
            ui.colored_label(palette().WARNING_FG, message);
        });
    }
    let mut remove = None;
    for (name, candidates) in &mut model.routes {
        ui.push_id(name, |ui| {
            ui.separator();
            let label = ui.label("Logical model name");
            let draft = model
                .route_name_edits
                .entry(name.clone())
                .or_insert_with(|| name.clone());
            ui.add(
                egui::TextEdit::singleline(draft)
                    .desired_width(ui.available_width())
                    .background_color(palette().INPUT),
            )
            .labelled_by(label.id);
            if model.pending_new_route.as_deref() == Some(name) {
                ui.label(muted("New route (not saved)"));
            }
            let users = model
                .route_users
                .get(draft)
                .filter(|users| !users.is_empty());
            ui.label(muted(users.map_or_else(
                || "Used by: none".into(),
                |users| format!("Used by: {}", users.join(", ")),
            )));
            if ui.button("Remove route").clicked() {
                remove = Some(name.clone());
            }
            let mut movement = None;
            let mut removal = None;
            let count = candidates.len();
            for (index, candidate) in candidates.iter_mut().enumerate() {
                ui.push_id(index, |ui| {
                    ui.label(muted(format!("Priority {}", index + 1)));
                    candidate_picker(
                        ui,
                        candidate,
                        (
                            &model.profile_names,
                            &model.profile_models,
                            &format!("{name} candidate {}", index + 1),
                        ),
                    );
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add_enabled(
                                index > 0,
                                egui::Button::new(format!("Move candidate {} up", index + 1)),
                            )
                            .clicked()
                        {
                            movement = Some((index, index - 1));
                        }
                        if ui
                            .add_enabled(
                                index + 1 < count,
                                egui::Button::new(format!("Move candidate {} down", index + 1)),
                            )
                            .clicked()
                        {
                            movement = Some((index, index + 1));
                        }
                        if ui
                            .button(format!("Remove candidate {}", index + 1))
                            .clicked()
                        {
                            removal = Some(index);
                        }
                    });
                });
            }
            if let Some((from, to)) = movement {
                candidates.swap(from, to);
            }
            if let Some(index) = removal {
                candidates.remove(index);
            }
            if ui.button("Add candidate").clicked() {
                candidates.push(config::RouteCandidateConfig {
                    profile: model.profile_names.first().cloned().unwrap_or_default(),
                    model: None,
                });
            }
        });
    }
    if let Some(name) = remove {
        model.routes.remove(&name);
        model.route_name_edits.remove(&name);
        if model.pending_new_route.as_deref() == Some(&name) {
            model.pending_new_route = None;
        }
    }
    ui.separator();
    let label = ui.label("New logical model name");
    ui.add(
        egui::TextEdit::singleline(&mut model.new_route_name)
            .desired_width(ui.available_width())
            .background_color(palette().INPUT),
    )
    .labelled_by(label.id);
    if ui.button("Add route").clicked() {
        match model.add_route(&model.new_route_name.clone()) {
            Ok(()) => {
                model.new_route_name.clear();
                model.validation_error = None;
            }
            Err(error) => model.validation_error = Some(error.to_string()),
        }
    }
}

fn candidate_picker(
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
}
