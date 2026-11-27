use std::collections::BTreeMap;

use crate::model::role_settings::{RoleSettingsModel, categories_for_role};
mod binding_status;
use crate::theme::{
    icons,
    text::{badge, muted},
    tokens::*,
    widgets::{icon_button, primary_button, surface_frame},
};
use binding_status::binding_status;

pub enum RoleSettingsAction {
    Save,
    CreateRoute(String),
    Profile(crate::model::role_profiles::RoleProfileAction),
}

/// The Agent roles tab. `profile_locked` disables profile changes while any settings tab
/// holds unsaved edits.
pub fn role_settings_body(
    ui: &mut egui::Ui,
    model: &mut RoleSettingsModel,
    profile_locked: bool,
) -> Option<RoleSettingsAction> {
    role_settings_content(ui, model, profile_locked, true)
}

/// The same body for a host that draws the profile control itself (the Model Settings
/// header) and scrolls the body: the bar and its locked notice are left out.
pub fn role_settings_hosted_body(
    ui: &mut egui::Ui,
    model: &mut RoleSettingsModel,
    profile_locked: bool,
) -> Option<RoleSettingsAction> {
    role_settings_content(ui, model, profile_locked, false)
}

fn role_settings_content(
    ui: &mut egui::Ui,
    model: &mut RoleSettingsModel,
    profile_locked: bool,
    profile_bar: bool,
) -> Option<RoleSettingsAction> {
    let mut action = None;
    let busy = model.is_saving();
    let ctx = ui.ctx().clone();
    {
        {
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            ui.label(muted(
                "Worker and reviewer categories take precedence over their role defaults; other roles use their role model.",
            ));
            if profile_bar {
                ui.add_enabled_ui(!busy && !profile_locked, |ui| {
                    if let Some(profile) = crate::panes::role_profiles::role_profile_bar(
                        ui,
                        "role",
                        &mut model.profiles,
                    ) {
                        action = Some(RoleSettingsAction::Profile(profile));
                    }
                });
                if profile_locked {
                    ui.label(muted(crate::panes::role_profiles::PROFILE_LOCKED_NOTICE));
                }
            }
            let scroll_height = if profile_bar {
                (ctx.viewport_rect().height() - TOPBAR * 7.0).max(TOPBAR)
            } else {
                f32::INFINITY
            };
            ui.add_enabled_ui(!busy, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("role-bindings")
                    .max_height(scroll_height)
                    .show(ui, |ui| {
                        if model.routes_empty {
                            surface_frame(palette().WARNING_SURFACE).show(ui, |ui| {
                                ui.colored_label(
                                    palette().WARNING_FG,
                                    "No routes configured. Roles without a matching route fail when used — assign models that have routes, or create routes in Routing settings.",
                                );
                            });
                        }
                        let conversation =
                            model.chain_routes(config::ChainPurpose::Conversation);
                        let image = model.chain_routes(config::ChainPurpose::ImageGeneration);
                        let no_efforts = BTreeMap::new();
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
                            ("Advisor", "advisor", &mut agents.roles.advisor, None),
                            ("Multimodal Looker", "multimodal_looker", &mut agents.roles.multimodal_looker, None),
                            ("Evaluator", "evaluator", &mut agents.roles.evaluator, None),
                        ] {
                            ui.push_id(name, |ui| {
                                ui.collapsing(badge(name), |ui| {
                                    chain_editor(
                                        ui,
                                        &mut binding.models,
                                        ChainChoices {
                                            routes: &conversation.heads,
                                            existing: &conversation.existing,
                                            misfits: &conversation.misfits,
                                            requirement: CHAT_ROUTES,
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
                                                        routes: &conversation.heads,
                                                        existing: &conversation.existing,
                                                        misfits: &conversation.misfits,
                                                        requirement: CHAT_ROUTES,
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
                                        routes: &conversation.heads,
                                        existing: &conversation.existing,
                                        misfits: &conversation.misfits,
                                        requirement: CHAT_ROUTES,
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
                        ui.push_id("image_generator", |ui| {
                            ui.collapsing(badge("Image generator"), |ui| {
                                let binding = &mut agents.roles.image_generator;
                                chain_editor(
                                    ui,
                                    &mut binding.models,
                                    ChainChoices {
                                        routes: &image.heads,
                                        existing: &image.existing,
                                        misfits: &image.misfits,
                                        requirement: IMAGE_ROUTES,
                                        efforts: &no_efforts,
                                        name: "Image generator route",
                                        inherit_label: IMAGE_GENERATION_DISABLED,
                                    },
                                );
                                if let Some(logical) = binding_status(
                                    ui,
                                    binding.head_route(),
                                    &model.route_names,
                                ) {
                                    action = Some(RoleSettingsAction::CreateRoute(logical));
                                }
                                ui.label(muted(IMAGE_ROUTES));
                            });
                        });
                    });
            });
            if let Some(error) = &model.error {
                crate::theme::widgets::reason_label(ui, palette().ERROR_FG, error);
            }
            if busy {
                ui.label(muted("Saving and reloading runtime..."));
            }
            ui.add_enabled_ui(!busy, |ui| {
                if primary_button(ui, "Save role settings").clicked() {
                    action = Some(RoleSettingsAction::Save);
                }
            });
        }
    }
    action
}

/// Shown when the image generator has no chain: image generation is off (ADR 0038).
pub const IMAGE_GENERATION_DISABLED: &str = "Disabled (generate_image unavailable)";
/// What conversation and compactor chains accept.
pub(crate) const CHAT_ROUTES: &str =
    "Only routes of chat models are offered for agent roles and the compactor.";
/// What the image generator chain accepts.
pub(crate) const IMAGE_ROUTES: &str =
    "Only routes of image generation models on Codex or OpenAI profiles are offered.";

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
        misfits,
        requirement,
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
                if icon_button(ui, icons::ARROW_UP, &format!("Move {entry_name} up")).clicked() {
                    movement = Some((index, index - 1));
                }
                let down = ui.add_enabled_ui(index + 1 < count, |ui| {
                    icon_button(ui, icons::ARROW_DOWN, &format!("Move {entry_name} down"))
                });
                if down.inner.clicked() {
                    movement = Some((index, index + 1));
                }
                if icon_button(ui, icons::TRASH, &format!("Remove {entry_name}")).clicked() {
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
    for entry in models.iter().filter(|entry| misfits.contains(&entry.route)) {
        ui.colored_label(
            palette().ERROR_FG,
            format!(
                "Route `{}` has models of another kind. {requirement}",
                entry.route
            ),
        );
    }
    let next = existing
        .iter()
        .find(|route| !models.iter().any(|entry| &entry.route == *route));
    let add = ui.add_enabled_ui(!models.is_empty() && next.is_some(), |ui| {
        icon_button(ui, icons::PLUS, &format!("Add {name} fallback"))
    });
    if add
        .inner
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
    /// Routes whose model kinds do not fit this chain (ADR 0038); entries naming one
    /// stay visible and are flagged instead of being hidden.
    misfits: &'a std::collections::BTreeSet<String>,
    /// What the chain accepts, shown next to a flagged entry.
    requirement: &'a str,
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
pub(crate) fn effort_picker(
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

pub(crate) fn optional_text(ui: &mut egui::Ui, label: &str, value: &mut Option<String>) {
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

pub(crate) fn generation(ui: &mut egui::Ui, value: &mut config::GenerationOverridesConfig) {
    optional_number(ui, "Temperature", (&mut value.temperature, 0.0..=2.0));
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
