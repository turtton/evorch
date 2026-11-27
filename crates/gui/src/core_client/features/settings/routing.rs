//! The Routing section of Model Settings: a pinned bar (filter, Add route), then one card per
//! route. A card says on its closed row what it holds (its candidates), who uses it and what
//! is wrong with it; the one open card is the route Save and Discard are for.

use super::rows::{
    chip, click_row, error_chip, muted_chip, named_combo, named_row, route_chip, warning_chip,
};
use crate::model::role_settings::ROLES;
use crate::model::routing_settings::{ModelChoices, RouteCandidateConfig, RoutingSettingsModel};
use crate::theme::{
    icons,
    text::{medium, muted},
    tokens::*,
    widgets::{ghost_icon_button, icon_button, primary_icon_button, reason_label, surface_frame},
};
use egui::{Margin, vec2};

/// What a route's controls ask of Model Settings beyond editing the draft.
pub(super) enum RouteAction {
    /// Delete the saved route `key` (after its confirmation).
    Delete(String),
}

#[derive(Default)]
pub(super) struct RoutingUi {
    filter: String,
    /// The route whose name is being typed, and the text typed so far.
    renaming: Option<String>,
    name_text: String,
    focus_name: bool,
    /// The saved route whose deletion waits for its confirmation.
    confirm_delete: Option<String>,
}

impl RoutingUi {
    /// The name field of the route `key` is opened and takes the keyboard.
    pub(super) fn rename_new(&mut self, key: &str) {
        self.renaming = Some(key.to_owned());
        self.name_text = key.to_owned();
        self.focus_name = true;
    }

    /// Takes the name typed for `key` into the draft when its field is still open, so a save
    /// does not leave the placeholder behind.
    ///
    /// # Errors
    /// The typed name is blank or taken.
    pub(super) fn commit_name(
        &mut self,
        model: &mut RoutingSettingsModel,
        key: &str,
    ) -> Result<(), String> {
        if self.renaming.as_deref() != Some(key) {
            return Ok(());
        }
        model
            .set_route_name(key, &self.name_text)
            .map_err(|error| error.to_string())?;
        self.renaming = None;
        Ok(())
    }

    pub(super) fn stop_editing(&mut self, key: &str) {
        if self.renaming.as_deref() == Some(key) {
            self.renaming = None;
        }
        if self.confirm_delete.as_deref() == Some(key) {
            self.confirm_delete = None;
        }
    }
}

/// What one card shows without being opened, computed before the cards borrow the model.
struct RouteInfo {
    key: String,
    name: String,
    /// `profile · model` of each candidate in order, and whether it was added automatically.
    candidates: Vec<(String, bool)>,
    users: Vec<String>,
    implicit_users: Vec<String>,
    mixed: bool,
    dirty: bool,
    new: bool,
    conflicted: bool,
}

/// The name of a role or category as the list shows it, from its address in the settings file.
fn address_label(address: &str) -> String {
    if let Some(role) = ROLES.iter().find(|role| role.address == address) {
        return role.label.to_owned();
    }
    for owner in ["worker", "reviewer"] {
        if let Some(category) = address.strip_prefix(&format!("{owner}.categories.")) {
            let label = ROLES
                .iter()
                .find(|role| role.key == owner)
                .map_or(owner, |role| role.label);
            return format!("{label} / {category}");
        }
    }
    address.to_owned()
}

fn infos(model: &RoutingSettingsModel) -> Vec<RouteInfo> {
    let target = |candidate: &RouteCandidateConfig| {
        let model_id = candidate.model.clone().or_else(|| {
            model
                .providers
                .get(&candidate.profile)
                .map(|profile| profile.default_model.clone())
        });
        match model_id {
            Some(id) if !id.is_empty() => format!("{} · {id}", candidate.profile),
            _ => candidate.profile.clone(),
        }
    };
    // A route that was never saved comes first, in the order it was added.
    let new = model.routes.keys().filter(|key| model.route_is_new(key));
    let saved = model.routes.keys().filter(|key| !model.route_is_new(key));
    new.chain(saved)
        .map(|key| {
            let candidates = &model.routes[key];
            RouteInfo {
                key: key.clone(),
                name: model.shown_name(key).to_owned(),
                candidates: candidates
                    .iter()
                    .map(|candidate| (target(candidate), candidate.auto))
                    .collect(),
                users: model
                    .explicit_users(key)
                    .iter()
                    .map(|address| address_label(address))
                    .collect(),
                implicit_users: model
                    .implicit_users(key)
                    .iter()
                    .map(|address| address_label(address))
                    .collect(),
                mixed: model.mixes_kinds(candidates),
                dirty: model.route_is_dirty(key),
                new: model.route_is_new(key),
                conflicted: model.conflicted.contains(key),
            }
        })
        .collect()
}

impl RouteInfo {
    fn all_users(&self) -> Vec<String> {
        let mut users = self.users.clone();
        for user in &self.implicit_users {
            if !users.contains(user) {
                users.push(user.clone());
            }
        }
        users
    }

    fn matches(&self, filter: &str) -> bool {
        let filter = filter.trim().to_lowercase();
        filter.is_empty()
            || self.name.to_lowercase().contains(&filter)
            || self.key.to_lowercase().contains(&filter)
            || self
                .candidates
                .iter()
                .any(|(target, _)| target.to_lowercase().contains(&filter))
            || self
                .all_users()
                .iter()
                .any(|user| user.to_lowercase().contains(&filter))
    }
}

/// Draws the section into the body of the modal, which is `height` tall: the bar stays where
/// it is and the cards scroll under it.
pub(super) fn routing_body(
    ui: &mut egui::Ui,
    model: &mut RoutingSettingsModel,
    state: &mut RoutingUi,
    height: f32,
) -> Option<RouteAction> {
    let mut action = None;
    ui.spacing_mut().item_spacing = vec2(SP_2, SP_2);
    let infos = infos(model);
    let unused = infos
        .iter()
        .filter(|info| info.all_users().is_empty())
        .count();
    let mut add = false;
    ui.horizontal(|ui| {
        let filter = ui.add(
            egui::TextEdit::singleline(&mut state.filter)
                .hint_text(icons::with_icon(icons::MAGNIFYING_GLASS, "Filter routes"))
                .desired_width(240.0)
                .background_color(palette().INPUT),
        );
        filter.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "Filter routes")
        });
        let routes = if infos.len() == 1 { "route" } else { "routes" };
        let summary = if unused == 0 {
            format!("{} {routes}", infos.len())
        } else {
            format!("{} {routes} · {unused} unused", infos.len())
        };
        ui.label(muted(summary));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            add = primary_icon_button(ui, icons::PLUS, "Add route").clicked();
        });
    });
    if add {
        let key = model.add_draft_route();
        state.rename_new(&key);
        state.filter.clear();
    }
    for message in [&model.validation_error, &model.conflict]
        .into_iter()
        .flatten()
    {
        reason_label(ui, palette().ERROR_FG, message);
    }
    let used = ui.min_rect().height();
    let scroll_height = (height - used - SP_2).max(ROW_DENSE * 3.0);
    egui::ScrollArea::vertical()
        .id_salt("routing-cards")
        .auto_shrink([false, false])
        .max_height(scroll_height)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = vec2(SP_2, SP_2);
            if model.routes_empty && infos.is_empty() {
                no_routes(ui, model);
            }
            let shown: Vec<_> = infos
                .iter()
                .filter(|info| info.matches(&state.filter))
                .collect();
            if shown.is_empty() && !infos.is_empty() {
                ui.label(muted("No route matches the filter."));
            }
            for info in shown {
                if let Some(next) = card(ui, model, state, info) {
                    action = Some(next);
                }
            }
        });
    action
}

fn no_routes(ui: &mut egui::Ui, model: &RoutingSettingsModel) {
    surface_frame(palette().WARNING_SURFACE).show(ui, |ui| {
        let message = if model.profile_names.is_empty() {
            "No provider profiles and no routes: logical models cannot resolve."
        } else {
            "No routes configured. Logical models without an explicit route fail when used. Add a route for each role or custom name."
        };
        ui.colored_label(palette().WARNING_FG, message);
    });
}

fn card(
    ui: &mut egui::Ui,
    model: &mut RoutingSettingsModel,
    state: &mut RoutingUi,
    info: &RouteInfo,
) -> Option<RouteAction> {
    let open = model.selected.as_deref() == Some(info.key.as_str());
    let mut action = None;
    let mut toggle = false;
    egui::Frame::new()
        .fill(palette().SURFACE)
        .stroke(egui::Stroke::new(
            1.0,
            if open {
                palette().ACCENT
            } else {
                palette().BORDER
            },
        ))
        .corner_radius(egui::CornerRadius::same(R_MD))
        .inner_margin(Margin::symmetric(SP_2 as i8, SP_1 as i8))
        .show(ui, |ui| {
            ui.push_id(&info.key, |ui| {
                let response = click_row(
                    ui,
                    ROW_DENSE + SP_1,
                    false,
                    |ui| closed_row_left(ui, info, open),
                    |ui| row_right(ui, model, state, info, open),
                );
                named_row(&response, &format!("Route {}", info.name));
                toggle = response.clicked();
                if open {
                    ui.add_space(SP_1);
                    if let Some(next) = card_body(ui, model, state, info) {
                        action = Some(next);
                    }
                }
            });
        });
    if toggle {
        model.selected = (!open).then(|| info.key.clone());
        if open {
            state.stop_editing(&info.key);
        }
    }
    action
}

fn closed_row_left(ui: &mut egui::Ui, info: &RouteInfo, open: bool) {
    let caret = if open {
        icons::CARET_DOWN
    } else {
        icons::CARET_RIGHT
    };
    ui.label(egui::RichText::new(caret).color(palette().TEXT_MUTED));
    ui.label(medium(&info.name).color(palette().TEXT));
    if open {
        return;
    }
    for (index, (target, auto)) in info.candidates.iter().enumerate() {
        if index > 0 {
            ui.label(egui::RichText::new(icons::ARROW_RIGHT).color(palette().TEXT_MUTED));
        }
        route_chip(
            ui,
            format!(
                "{} {target}{}",
                index + 1,
                if *auto { " · auto" } else { "" }
            ),
        );
    }
}

/// The right end of a route's row: what is wrong or unsaved, who uses it, and for the open
/// one the rename and delete icons (drawn right to left, so these come first).
fn row_right(
    ui: &mut egui::Ui,
    model: &mut RoutingSettingsModel,
    state: &mut RoutingUi,
    info: &RouteInfo,
    open: bool,
) {
    if open {
        if icon_button(ui, icons::TRASH, &format!("Delete route {}", info.name)).clicked() {
            if info.new {
                model.discard_route(&info.key);
                state.stop_editing(&info.key);
            } else {
                state.confirm_delete = Some(info.key.clone());
            }
        }
        if state.renaming.as_deref() != Some(info.key.as_str())
            && icon_button(
                ui,
                icons::PENCIL_SIMPLE,
                &format!("Rename route {}", info.name),
            )
            .clicked()
        {
            state.renaming = Some(info.key.clone());
            state.name_text.clone_from(&info.name);
            state.focus_name = true;
        }
    }
    if info.conflicted {
        error_chip(ui, "Changed elsewhere");
    } else if info.new {
        chip(ui, "New", palette().INFO, palette().UNREAD_SURFACE);
    } else if info.dirty {
        warning_chip(ui, "Unsaved");
    }
    if info.mixed {
        error_chip(
            ui,
            format!("{} Mixes chat and image models", icons::WARNING),
        );
    }
    let users = info.all_users();
    if users.is_empty() {
        muted_chip(ui, "Unused");
    } else {
        // Drawn from the right: the count of those not named comes first.
        if users.len() > 2 {
            muted_chip(ui, format!("+{}", users.len() - 2));
        }
        for user in users.iter().take(2).rev() {
            route_chip(ui, user);
        }
    }
}

fn card_body(
    ui: &mut egui::Ui,
    model: &mut RoutingSettingsModel,
    state: &mut RoutingUi,
    info: &RouteInfo,
) -> Option<RouteAction> {
    let mut action = None;
    if state.renaming.as_deref() == Some(info.key.as_str()) {
        name_editor(ui, model, state, info);
    }
    if info.conflicted {
        reason_label(
            ui,
            palette().ERROR_FG,
            "This route was changed elsewhere while it had unsaved edits, so it is not saved over the newer one. Discard these edits to use the saved route.",
        );
    }
    if info.mixed {
        reason_label(
            ui,
            palette().ERROR_FG,
            "This route mixes chat and image generation models; keep one model kind per route.",
        );
    }
    let typed_name = if state.renaming.as_deref() == Some(info.key.as_str()) {
        state.name_text.trim()
    } else {
        info.name.as_str()
    };
    if typed_name != info.key && !info.implicit_users.is_empty() {
        reason_label(
            ui,
            palette().WARNING_FG,
            format!(
                "Renaming leaves implicit role-name lookups unrouted: {}. Assign them explicitly in Agent Roles to keep them working.",
                info.implicit_users.join(", ")
            ),
        );
    }
    if state.confirm_delete.as_deref() == Some(info.key.as_str()) {
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(
                palette().WARNING_FG,
                format!(
                    "Delete route {}? {}",
                    info.name,
                    if info.users.is_empty() {
                        "Nothing uses it."
                    } else {
                        "The roles that use it will have no route."
                    }
                ),
            );
            if ui.button("Delete route").clicked() {
                state.confirm_delete = None;
                action = Some(RouteAction::Delete(info.key.clone()));
            }
            if ui.button("Cancel").clicked() {
                state.confirm_delete = None;
            }
        });
    }
    candidates(ui, model, info);
    action
}

fn name_editor(
    ui: &mut egui::Ui,
    model: &mut RoutingSettingsModel,
    state: &mut RoutingUi,
    info: &RouteInfo,
) {
    ui.horizontal(|ui| {
        let response = ui.add(
            egui::TextEdit::singleline(&mut state.name_text)
                .desired_width(240.0)
                .background_color(palette().INPUT),
        );
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "Logical model name")
        });
        if state.focus_name {
            // Focused with the whole name selected, so typing replaces a placeholder.
            response.request_focus();
            if let Some(mut edit) = egui::TextEdit::load_state(ui.ctx(), response.id) {
                let end = egui::text::CCursor::new(state.name_text.chars().count());
                edit.cursor
                    .set_char_range(Some(egui::text::CCursorRange::two(
                        egui::text::CCursor::new(0),
                        end,
                    )));
                edit.store(ui.ctx(), response.id);
            }
            state.focus_name = false;
        }
        let entered =
            response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
        let accept = icon_button(ui, icons::CHECK, "Accept name").clicked() || entered;
        let cancel = icon_button(ui, icons::X, "Cancel rename").clicked();
        if accept {
            match model.set_route_name(&info.key, &state.name_text) {
                Ok(()) => {
                    model.validation_error = None;
                    state.renaming = None;
                }
                Err(error) => model.validation_error = Some(error.to_string()),
            }
        } else if cancel {
            model.route_name_edits.remove(&info.key);
            model.validation_error = None;
            state.renaming = None;
        }
    });
}

/// The candidates of the open route as a table: order, provider, model, auto, and the arrows
/// that reorder them (the only way to).
fn candidates(ui: &mut egui::Ui, model: &mut RoutingSettingsModel, info: &RouteInfo) {
    let key = info.key.as_str();
    let count = model.routes.get(key).map_or(0, Vec::len);
    let choices: Vec<ModelChoices> = (0..count)
        .map(|index| model.model_choices(key, index))
        .collect();
    let profile_names = model.profile_names.clone();
    let fresh = model.new_candidate(key);
    let mut refit = None;
    let mut movement = None;
    let mut removal = None;
    let mut append = false;
    if let Some(candidates) = model.routes.get_mut(key) {
        egui::Grid::new(("candidates", key))
            .num_columns(5)
            .spacing(vec2(SP_3, SP_2))
            .show(ui, |ui| {
                for heading in ["#", "Provider", "Model", "", "Order"] {
                    ui.label(muted(heading));
                }
                ui.end_row();
                for (index, candidate) in candidates.iter_mut().enumerate() {
                    let name = format!("{} candidate {}", info.name, index + 1);
                    ui.label(muted((index + 1).to_string()));
                    let mut selected = candidate.profile.clone();
                    let combo = egui::ComboBox::from_id_salt(("profile", index))
                        .width(150.0)
                        .selected_text(&candidate.profile)
                        .show_ui(ui, |ui| {
                            for profile in &profile_names {
                                ui.selectable_value(&mut selected, profile.clone(), profile);
                            }
                        });
                    named_combo(&combo.response, &format!("{name} profile"));
                    if selected != candidate.profile {
                        refit = Some((index, selected));
                    }
                    let available = &choices[index];
                    let combo = egui::ComboBox::from_id_salt(("model", index))
                        .width(220.0)
                        .selected_text(candidate.model.as_deref().unwrap_or("(profile default)"))
                        .show_ui(ui, |ui| {
                            if available.profile_default {
                                ui.selectable_value(
                                    &mut candidate.model,
                                    None,
                                    "(profile default)",
                                );
                            }
                            for model_id in &available.models {
                                ui.selectable_value(
                                    &mut candidate.model,
                                    Some(model_id.clone()),
                                    model_id,
                                );
                            }
                        });
                    named_combo(&combo.response, &format!("{name} model override"));
                    if candidate.auto {
                        muted_chip(ui, "auto")
                            .on_hover_text("Added automatically for the same models.dev model");
                    } else {
                        ui.label("");
                    }
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(index > 0, |ui| {
                            if icon_button(
                                ui,
                                icons::ARROW_UP,
                                &format!("Move candidate {} up", index + 1),
                            )
                            .clicked()
                            {
                                movement = Some((index, index - 1));
                            }
                        });
                        ui.add_enabled_ui(index + 1 < count, |ui| {
                            if icon_button(
                                ui,
                                icons::ARROW_DOWN,
                                &format!("Move candidate {} down", index + 1),
                            )
                            .clicked()
                            {
                                movement = Some((index, index + 1));
                            }
                        });
                        if icon_button(ui, icons::TRASH, &format!("Remove candidate {}", index + 1))
                            .clicked()
                        {
                            removal = Some(index);
                        }
                    });
                    ui.end_row();
                }
            });
        if let Some((from, to)) = movement {
            candidates.swap(from, to);
        }
        if let Some(index) = removal {
            candidates.remove(index);
        }
        if ghost_icon_button(ui, icons::PLUS, "Add candidate").clicked() {
            append = true;
        }
        if append {
            candidates.push(fresh);
        }
    }
    if let Some((index, profile)) = refit {
        if model.change_candidate_profile(key, index, &profile) {
            model.validation_error = None;
        } else {
            model.validation_error = Some(format!(
                "Profile `{profile}` has no model matching route `{}`; profile change cancelled",
                info.name
            ));
        }
    }
}
