use std::collections::BTreeMap;

use crate::model::role_settings::{RoleSettingsModel, categories_for_role};
mod binding_status;
use crate::theme::{
    text::{badge, h3, muted},
    tokens::*,
    widgets::{primary_button, surface_frame},
};
use binding_status::binding_status;

pub enum RoleSettingsAction {
    Save,
    Cancel,
    CreateRoute(String),
    Profile(crate::model::role_profiles::RoleProfileAction),
}

pub fn role_settings_modal(
    ctx: &egui::Context,
    model: &mut RoleSettingsModel,
) -> Option<RoleSettingsAction> {
    let mut action = None;
    let busy = model.is_saving();
    egui::Modal::new(egui::Id::new("role-settings"))
        .backdrop_color(palette().OVERLAY)
        .frame(surface_frame(palette().SURFACE_RAISED))
        .show(ctx, |ui| {
            ui.set_width(
                (ctx.viewport_rect().width() * 0.6).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 4.0,
            );
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            ui.label(h3("Agent role settings"));
            ui.label(muted(
                "Worker and reviewer categories take precedence over their role defaults; other roles use their role model.",
            ));
            ui.add_enabled_ui(!busy, |ui| {
                if let Some(profile) =
                    crate::panes::role_profiles::role_profile_bar(ui, "role", &mut model.profiles)
                {
                    action = Some(RoleSettingsAction::Profile(profile));
                }
            });
            ui.add_enabled_ui(!busy, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("role-bindings")
                    .max_height((ctx.viewport_rect().height() - TOPBAR * 4.0).max(TOPBAR))
                    .show(ui, |ui| {
                        if model.routes_empty {
                            surface_frame(palette().WARNING_SURFACE).show(ui, |ui| {
                                ui.colored_label(
                                    palette().WARNING_FG,
                                    "No routes configured. Roles without a matching route fail when used — assign models that have routes, or create routes in Routing settings.",
                                );
                            });
                        }
                        let agents = &mut model.agents;
                        for (name, role_key, binding, categories) in [
                            ("Orchestrator", "orchestrator", &mut agents.orchestrator, None),
                            ("Explorer", "explorer", &mut agents.explorer, None),
                            (
                                "Worker",
                                "worker",
                                &mut agents.worker.base,
                                Some(&mut agents.worker.categories),
                            ),
                            ("Reviewer", "reviewer", &mut agents.reviewer.base, Some(&mut agents.reviewer.categories)),
                            ("WebResearcher", "web_researcher", &mut agents.roles.web_researcher, None),
                            ("Planner", "planner", &mut agents.roles.planner, None),
                            ("Oracle", "oracle", &mut agents.roles.oracle, None),
                            ("Multimodal Looker", "multimodal_looker", &mut agents.roles.multimodal_looker, None),
                        ] {
                            ui.push_id(name, |ui| {
                                ui.collapsing(badge(name), |ui| {
                                    chain_editor(
                                        ui,
                                        &mut binding.models,
                                        ChainChoices {
                                            routes: &model.logical_models,
                                            existing: &model.route_names,
                                            efforts: &model.route_efforts,
                                            name: "Role route",
                                            inherit_label: "Inherit default",
                                        },
                                    );
                                    let effective = effective_route(binding.head_route(), None, role_key);
                                    if let Some(logical) = binding_status(ui, Some(effective), &model.route_names) {
                                        action = Some(RoleSettingsAction::CreateRoute(logical));
                                    }
                                    if let Some(Some(resolved)) = model.resolved_previews.get(name) {
                                        ui.label(muted(format!("→ {resolved}")));
                                    }
                                    optional_text(ui, "Preset reference", &mut binding.preset);
                                    ui.collapsing(badge("Generation overrides"), |ui| {
                                        generation(ui, &mut binding.generation)
                                    });
                                    if let Some(categories) = categories {
                                        ui.label(muted("Category overrides"));
                                        for category in categories_for_role(role_key).map(|category| category.id.as_str()) {
                                            ui.collapsing(badge(category), |ui| {
                                                let mut draft = categories
                                                    .get(category)
                                                    .cloned()
                                                    .unwrap_or_default();
                                                chain_editor(
                                                    ui,
                                                    &mut draft.models,
                                                    ChainChoices {
                                                        routes: &model.logical_models,
                                                        existing: &model.route_names,
                                                        efforts: &model.route_efforts,
                                                        name: &format!("{category} route"),
                                                        inherit_label: "Inherit default",
                                                    },
                                                );
                                                let effective = effective_route(
                                                    draft.head_route(),
                                                    binding.head_route(),
                                                    role_key,
                                                );
                                                if let Some(logical) = binding_status(ui, Some(effective), &model.route_names) {
                                                    action = Some(RoleSettingsAction::CreateRoute(logical));
                                                }
                                                if let Some(Some(resolved)) = model.resolved_previews.get(&format!("{role_key}.categories.{category}")) {
                                                    ui.label(muted(format!("→ {resolved}")));
                                                }
                                                optional_text(
                                                    ui,
                                                    "Category preset reference",
                                                    &mut draft.preset,
                                                );
                                                generation(ui, &mut draft.generation);
                                                if ui.button("Reset category").clicked() {
                                                    draft = Default::default();
                                                }
                                                if draft == config::CategoryBindingConfig::default() {
                                                    categories.remove(category);
                                                } else {
                                                    categories.insert(category.into(), draft);
                                                }
                                            });
                                        }
                                    }
                                });
                            });
                        }
                        ui.push_id("compactor", |ui| {
                            ui.collapsing(badge("Compactor (fallback)"), |ui| {
                                let binding = &mut agents.roles.compactor;
                                chain_editor(
                                    ui,
                                    &mut binding.models,
                                    ChainChoices {
                                        routes: &model.logical_models,
                                        existing: &model.route_names,
                                        efforts: &model.route_efforts,
                                        name: "Compactor route",
                                        inherit_label: "Use running role's chain",
                                    },
                                );
                                if let Some(logical) = binding_status(
                                    ui,
                                    binding.head_route(),
                                    &model.route_names,
                                ) {
                                    action = Some(RoleSettingsAction::CreateRoute(logical));
                                }
                                ui.add_enabled_ui(!binding.models.is_empty(), |ui| {
                                    ui.collapsing(badge("Generation overrides"), |ui| {
                                        generation(ui, &mut binding.generation)
                                    });
                                });
                            });
                        });
                    });
            });
            if let Some(error) = &model.error {
                ui.colored_label(palette().ERROR_FG, error);
            }
            if busy {
                ui.label(muted("Saving and reloading runtime..."));
            }
            ui.add_enabled_ui(!busy, |ui| {
                ui.horizontal(|ui| {
                    if primary_button(ui, "Save role settings").clicked() {
                        action = Some(RoleSettingsAction::Save);
                    }
                    if ui.button("Cancel").clicked() {
                        action = Some(RoleSettingsAction::Cancel);
                    }
                })
            });
        });
    action
}

/// `AgentsConfig::binding_for` と同じ優先順で、編集中のチェーンの先頭 route を解決する。
fn effective_route<'a>(
    explicit: Option<&'a str>,
    inherited: Option<&'a str>,
    role_key: &'a str,
) -> &'a str {
    explicit.or(inherited).unwrap_or(role_key)
}

/// ロールのチェーンを編集する。先頭の route は未作成の名前も表示でき、後続の要素は
/// 既存の route からだけ選べる。各要素は推論強度を持ち、並べ替え・削除・追加ができる。
///
/// 2 要素以上のチェーンでは、チェーンごと消してしまわないよう継承 (空のチェーン) を選べない。
fn chain_editor(
    ui: &mut egui::Ui,
    models: &mut Vec<config::ChainEntryConfig>,
    choices: ChainChoices<'_>,
) {
    let ChainChoices {
        routes,
        existing,
        efforts,
        name,
        inherit_label,
    } = choices;
    let mut head = models.first().map(|entry| entry.route.clone());
    let can_inherit = models.len() <= 1;
    let label = ui.label(name);
    egui::ComboBox::from_id_salt(name)
        .width(ui.available_width())
        .selected_text(head.as_deref().unwrap_or(inherit_label))
        .show_ui(ui, |ui| {
            if can_inherit {
                ui.selectable_value(&mut head, None, inherit_label);
            }
            for route in routes {
                ui.selectable_value(&mut head, Some(route.clone()), route);
            }
        })
        .response
        .labelled_by(label.id);
    match (head, models.first_mut()) {
        (None, _) => models.clear(),
        (Some(route), Some(entry)) => entry.route = route,
        (Some(route), None) => models.push(config::ChainEntryConfig::route(route)),
    }
    if let Some(entry) = models.first_mut() {
        entry_effort(ui, entry, efforts, name);
    }
    let mut movement = None;
    let mut removal = None;
    let count = models.len();
    for (index, entry) in models.iter_mut().enumerate().skip(1) {
        let entry_name = format!("{name} fallback {index}");
        ui.push_id(&entry_name, |ui| {
            let label = ui.label(&entry_name);
            egui::ComboBox::from_id_salt(&entry_name)
                .width(ui.available_width())
                .selected_text(entry.route.as_str())
                .show_ui(ui, |ui| {
                    for route in existing {
                        ui.selectable_value(&mut entry.route, route.clone(), route);
                    }
                })
                .response
                .labelled_by(label.id);
            entry_effort(ui, entry, efforts, &entry_name);
            ui.horizontal_wrapped(|ui| {
                if ui.button(format!("Move {entry_name} up")).clicked() {
                    movement = Some((index, index - 1));
                }
                if ui
                    .add_enabled(
                        index + 1 < count,
                        egui::Button::new(format!("Move {entry_name} down")),
                    )
                    .clicked()
                {
                    movement = Some((index, index + 1));
                }
                if ui.button(format!("Remove {entry_name}")).clicked() {
                    removal = Some(index);
                }
            });
        });
    }
    if let Some((from, to)) = movement {
        models.swap(from, to);
    } else if let Some(index) = removal {
        models.remove(index);
    }
    let next = existing
        .iter()
        .find(|route| !models.iter().any(|entry| &entry.route == *route));
    if ui
        .add_enabled(
            !models.is_empty() && next.is_some(),
            egui::Button::new(format!("Add {name} fallback")),
        )
        .on_disabled_hover_text("Choose the first route, and define another route to fall back to")
        .clicked()
        && let Some(route) = next
    {
        models.push(config::ChainEntryConfig::route(route.clone()));
    }
}

/// The route lists and effort choices a chain editor offers.
#[derive(Clone, Copy)]
struct ChainChoices<'a> {
    /// Every name a head may show, including routes not created yet.
    routes: &'a [String],
    /// Routes that exist, the only ones a fallback can pick.
    existing: &'a std::collections::BTreeSet<String>,
    efforts: &'a BTreeMap<String, Vec<String>>,
    name: &'a str,
    inherit_label: &'a str,
}

/// The entry's reasoning-effort picker, shown when its route offers levels or one is set.
fn entry_effort(
    ui: &mut egui::Ui,
    entry: &mut config::ChainEntryConfig,
    efforts: &BTreeMap<String, Vec<String>>,
    name: &str,
) {
    let choices = efforts.get(&entry.route).map_or(&[][..], Vec::as_slice);
    if !choices.is_empty() || entry.reasoning_effort.is_some() {
        let label = ui.label(format!("{name} reasoning effort"));
        effort_picker(
            ui,
            (name, "reasoning-effort"),
            "(provider default)",
            &mut entry.reasoning_effort,
            choices,
        )
        .labelled_by(label.id);
    }
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

fn optional_text(ui: &mut egui::Ui, label: &str, value: &mut Option<String>) {
    let label = ui.label(label);
    let mut text = value.clone().unwrap_or_default();
    if ui
        .add(
            egui::TextEdit::singleline(&mut text)
                .desired_width(ui.available_width())
                .background_color(palette().INPUT)
                .hint_text("Inherit default"),
        )
        .labelled_by(label.id)
        .changed()
    {
        *value = (!text.trim().is_empty()).then(|| text.trim().into());
    }
}

fn generation(ui: &mut egui::Ui, value: &mut config::GenerationOverridesConfig) {
    optional_number(ui, "Temperature", (&mut value.temperature, 0.0..=2.0));
    optional_number(ui, "Top p", (&mut value.top_p, 0.0..=1.0));
    optional_number(ui, "Max tokens", (&mut value.max_tokens, 1..=u32::MAX));
}

fn optional_number<T: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    label: &str,
    field: (&mut Option<T>, std::ops::RangeInclusive<T>),
) {
    ui.horizontal(|ui| {
        let mut enabled = field.0.is_some();
        if ui.checkbox(&mut enabled, label).changed() {
            *field.0 = enabled.then(|| *field.1.start());
        }
        if let Some(value) = field.0 {
            ui.add(egui::DragValue::new(value).range(field.1));
        }
    });
}
