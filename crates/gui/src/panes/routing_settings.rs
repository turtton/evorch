use crate::model::routing_settings::RoutingSettingsModel;
mod candidate;
use crate::theme::{
    text::{h3, muted},
    tokens::*,
    widgets::{primary_button, surface_frame},
};
use candidate::candidate_picker;

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
                        model.origin_role_settings = false;
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
                .profile_defaults
                .keys()
                .next()
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
            let expanded = model.expanded.contains(name);
            let header = egui::CollapsingHeader::new(name)
                .open(Some(expanded))
                .show(ui, |ui| {
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
                                        egui::Button::new(format!(
                                            "Move candidate {} up",
                                            index + 1
                                        )),
                                    )
                                    .clicked()
                                {
                                    movement = Some((index, index - 1));
                                }
                                if ui
                                    .add_enabled(
                                        index + 1 < count,
                                        egui::Button::new(format!(
                                            "Move candidate {} down",
                                            index + 1
                                        )),
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
            if header.header_response.clicked() {
                if expanded {
                    model.expanded.remove(name);
                } else {
                    model.expanded.insert(name.clone());
                }
            }
            let draft = model.route_name_edits.get(name).unwrap_or(name);
            let users = model
                .route_users
                .get(draft)
                .filter(|users| !users.is_empty());
            ui.label(muted(users.map_or_else(
                || "Used by: none".into(),
                |users| format!("Used by: {}", users.join(", ")),
            )));
        });
    }
    if let Some(name) = remove {
        model.routes.remove(&name);
        model.route_name_edits.remove(&name);
        model.expanded.remove(&name);
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
