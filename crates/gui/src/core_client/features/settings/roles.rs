//! The Agent Roles section of Model Settings: every role in one table, grouped as Core
//! agents, Specialists and System, with its chain, what the chain resolves to and its
//! warnings, and the role picked in the table edited in the pane beside it. Save and Discard
//! are for that one role.

use super::rows::{
    block_title, chip, click_row, error_chip, muted_chip, named_combo, named_row, route_chip,
    warning_chip,
};
use crate::model::role_settings::{
    CategoryBindingConfig, ChainEntryConfig, ChainPurpose, ChainRoutes, ROLES, RoleBindingConfig,
    RoleGroup, RoleInfo, RoleSettingsModel, categories_for_role, parts, parts_mut,
};
use crate::panes::role_settings::{
    CHAT_ROUTES, IMAGE_GENERATION_DISABLED, IMAGE_ROUTES, effort_picker, generation, optional_text,
};
use crate::theme::{
    icons,
    text::{h3, medium, muted},
    tokens::*,
    widgets::{accessible, ghost, icon_button, reason_label},
};
use egui::{Rect, Sense, UiBuilder, vec2};
use std::collections::{BTreeMap, BTreeSet};

/// What the pane asks of Model Settings beyond editing the draft.
pub(super) enum RolesAction {
    /// Open Routing on a new route of this name, and come back once it is saved.
    CreateRoute(String),
}

#[derive(Default)]
pub(super) struct RolesUi {
    /// The category whose override is open for editing in the pane.
    editing: Option<String>,
    /// The chain's first route takes the keyboard on the next frame ("Choose an existing route").
    focus_head: bool,
}

impl RolesUi {
    pub(super) fn stop_editing(&mut self) {
        self.editing = None;
    }
}

/// What the table and the pane read of the model while the draft is being edited.
struct Context {
    conversation: ChainRoutes,
    image: ChainRoutes,
    efforts: BTreeMap<String, Vec<String>>,
    route_names: BTreeSet<String>,
    /// What each route resolves to now: its first candidate, `profile · model`.
    targets: BTreeMap<String, String>,
}

impl Context {
    fn of(model: &RoleSettingsModel) -> Self {
        Self {
            conversation: model.chain_routes(ChainPurpose::Conversation),
            image: model.chain_routes(ChainPurpose::ImageGeneration),
            efforts: model.route_efforts.clone(),
            route_names: model.route_names.clone(),
            targets: model
                .routes
                .keys()
                .filter_map(|route| Some((route.clone(), model.resolved_target(route)?)))
                .collect(),
        }
    }

    fn routes_for(&self, role: &RoleInfo) -> &ChainRoutes {
        if role.is_image_generator() {
            &self.image
        } else {
            &self.conversation
        }
    }
}

/// Draws the section into the body of the modal, which is `height` tall: the table on the left
/// and the selected role on the right, each scrolling on its own.
pub(super) fn roles_body(
    ui: &mut egui::Ui,
    model: &mut RoleSettingsModel,
    state: &mut RolesUi,
    stale: Option<&str>,
    height: f32,
) -> Option<RolesAction> {
    let mut action = None;
    ui.spacing_mut().item_spacing = vec2(SP_2, SP_2);
    for message in [stale, model.error.as_deref()].into_iter().flatten() {
        reason_label(ui, palette().ERROR_FG, message);
    }
    if model.routes_empty {
        ui.colored_label(
            palette().WARNING_FG,
            "No routes configured. Roles without a matching route fail when used. Create the routes in Routing.",
        );
    }
    let remaining = (height - ui.min_rect().height() - SP_2).max(ROW_DENSE * 6.0);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), remaining), Sense::hover());
    let list_width = (rect.width() * 0.56).clamp(260.0, (rect.width() - 280.0).max(260.0));
    let list = Rect::from_min_size(rect.min, vec2(list_width, rect.height()));
    let detail_rect = Rect::from_min_max(egui::pos2(list.max.x + SP_3, rect.min.y), rect.max);
    ui.painter().vline(
        list.max.x + SP_3 / 2.0,
        rect.y_range(),
        egui::Stroke::new(1.0, palette().BORDER),
    );
    let context = Context::of(model);
    let mut list_ui = ui.new_child(UiBuilder::new().max_rect(list));
    list_ui.set_clip_rect(list.intersect(ui.clip_rect()));
    egui::ScrollArea::vertical()
        .id_salt("roles-list")
        .auto_shrink([false, false])
        .show(&mut list_ui, |ui| role_table(ui, model, &context));
    let mut detail_ui = ui.new_child(UiBuilder::new().max_rect(detail_rect));
    detail_ui.set_clip_rect(detail_rect.intersect(ui.clip_rect()));
    egui::ScrollArea::vertical()
        .id_salt("roles-detail")
        .auto_shrink([false, false])
        .show(&mut detail_ui, |ui| {
            ui.spacing_mut().item_spacing = vec2(SP_2, SP_2);
            action = detail(ui, model, state, &context);
        });
    action
}

fn role_table(ui: &mut egui::Ui, model: &mut RoleSettingsModel, context: &Context) {
    ui.spacing_mut().item_spacing = vec2(SP_2, SP_1);
    ui.horizontal(|ui| {
        ui.add_space(SP_2);
        role_column(ui, ROW_DENSE - SP_2, muted("Role"));
        ui.label(muted("Model chain"));
    });
    for group in RoleGroup::ALL {
        ui.add_space(SP_2);
        let count = ROLES.iter().filter(|role| role.group == group).count();
        block_title(ui, &format!("{} · {count}", group.title()));
        for role in ROLES.iter().filter(|role| role.group == group) {
            let selected = model.selected == Some(role.address);
            let response = click_row(
                ui,
                ROW_DENSE + SP_2,
                selected,
                |ui| {
                    role_column(ui, ROW_DENSE, medium(role.label).color(palette().TEXT));
                    chain_summary(ui, model, context, role);
                },
                |ui| {
                    ui.label(egui::RichText::new(icons::CARET_RIGHT).color(palette().TEXT_MUTED));
                    if let (_, Some(categories)) = parts(&model.agents, role.address) {
                        let total = categories_for_role(role.key).count();
                        let set = categories_for_role(role.key)
                            .filter(|category| {
                                categories.get(category.id.as_str()).is_some_and(|binding| {
                                    *binding != CategoryBindingConfig::default()
                                })
                            })
                            .count();
                        if set > 0 {
                            chip(
                                ui,
                                format!("{set} / {total} set"),
                                palette().INFO,
                                palette().UNREAD_SURFACE,
                            );
                        } else {
                            muted_chip(ui, format!("0 / {total}"));
                        }
                    }
                    if model.conflicted.contains(role.address) {
                        error_chip(ui, "Changed elsewhere");
                    } else if model.role_is_dirty(role) {
                        warning_chip(ui, "Unsaved");
                    }
                },
            );
            named_row(&response, role.label);
            if response.clicked() {
                model.selected = Some(role.address);
            }
        }
    }
    ui.add_space(SP_2);
    ui.label(muted(
        "Boxes are route names; the grey text is what they resolve to (provider · model · effort). \"Inherits\" means the role uses the route named after it.",
    ));
}

const ROLE_COLUMN: f32 = 132.0;

/// The first column of a row: a name, left aligned in a fixed width.
fn role_column(ui: &mut egui::Ui, height: f32, text: egui::RichText) {
    ui.allocate_ui_with_layout(
        vec2(ROLE_COLUMN, height),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| ui.add(egui::Label::new(text).truncate()),
    );
}

/// The chain of a role on one line: its routes and what the first resolves to, or why it has
/// none. A compactor that inherits is not a problem; an image generator without a chain is off.
fn chain_summary(ui: &mut egui::Ui, model: &RoleSettingsModel, context: &Context, role: &RoleInfo) {
    let (binding, _) = parts(&model.agents, role.address);
    let explicit = !binding.models.is_empty();
    if !explicit && role.is_compactor() {
        ui.label(muted("Uses the running role's chain"));
        return;
    }
    if !explicit && role.is_image_generator() {
        ui.label(muted(IMAGE_GENERATION_DISABLED));
        return;
    }
    if !explicit {
        ui.label(muted("Inherits"));
    }
    let routes: Vec<&str> = if explicit {
        binding
            .models
            .iter()
            .map(|entry| entry.route.as_str())
            .collect()
    } else {
        vec![role.key]
    };
    for (index, route) in routes.iter().take(2).enumerate() {
        if index > 0 {
            ui.label(egui::RichText::new(icons::ARROW_RIGHT).color(palette().TEXT_MUTED));
        }
        route_chip(ui, *route);
    }
    if routes.len() > 2 {
        muted_chip(ui, format!("+{}", routes.len() - 2));
    }
    let head = routes[0];
    if routes
        .iter()
        .any(|route| !context.route_names.contains(*route))
    {
        warning_chip(ui, format!("{} No route", icons::WARNING));
        return;
    }
    if let Some(target) = context.targets.get(head) {
        let effort = binding
            .models
            .first()
            .and_then(|entry| entry.reasoning_effort.as_deref());
        ui.add(
            egui::Label::new(muted(match effort {
                Some(effort) => format!("{target} · {effort}"),
                None => target.clone(),
            }))
            .truncate(),
        );
    }
}

fn detail(
    ui: &mut egui::Ui,
    model: &mut RoleSettingsModel,
    state: &mut RolesUi,
    context: &Context,
) -> Option<RolesAction> {
    let Some(role) = ROLES
        .iter()
        .find(|role| Some(role.address) == model.selected)
    else {
        ui.label(muted("Select a role to edit it."));
        return None;
    };
    let mut action = None;
    let dirty = model.role_is_dirty(role);
    let effective = model.effective_route(role).to_owned();
    let explicit = parts(&model.agents, role.address).0.head_route().is_some();
    let missing = !context.route_names.contains(&effective)
        && (explicit || !(role.is_compactor() || role.is_image_generator()));
    let conflicted = model.conflicted.contains(role.address);
    let mut discard = false;
    ui.horizontal(|ui| {
        ui.label(h3(role.label));
        muted_chip(ui, role.group.singular());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if dirty {
                discard = icon_button(
                    ui,
                    icons::ARROW_COUNTER_CLOCKWISE,
                    "Discard changes to this role",
                )
                .clicked();
            }
        });
    });
    ui.label(muted(role.blurb));
    if conflicted {
        reason_label(
            ui,
            palette().ERROR_FG,
            "This role was changed elsewhere while it had unsaved edits, so it is not saved over the newer one. Discard these edits to use the saved role.",
        );
    }
    if missing {
        no_route(ui, role, &effective, state, &mut action);
    }
    block_title(ui, "Model chain");
    let routes = context.routes_for(role);
    let (requirement, inherit_label, chain_name) = if role.is_image_generator() {
        (
            IMAGE_ROUTES,
            IMAGE_GENERATION_DISABLED,
            "Image generator route",
        )
    } else if role.is_compactor() {
        (CHAT_ROUTES, "Use running role's chain", "Compactor route")
    } else {
        (CHAT_ROUTES, "Inherit default", "Role route")
    };
    let focus_head = std::mem::take(&mut state.focus_head);
    {
        let (binding, categories) = parts_mut(&mut model.agents, role.address);
        chain_editor(
            ui,
            &mut binding.models,
            &Chain {
                name: chain_name,
                routes,
                requirement,
                inherit_label,
                focus_head,
            },
            context,
        );
        if let Some(categories) = categories {
            category_overrides(ui, role, binding, categories, state, context, &mut action);
        }
        if !role.is_image_generator() {
            advanced(ui, role, binding);
        }
    }
    if discard {
        model.discard_role(role);
        state.stop_editing();
    }
    action
}

/// The role's route does not exist: say what happens, and offer to create it or pick another.
fn no_route(
    ui: &mut egui::Ui,
    role: &RoleInfo,
    route: &str,
    state: &mut RolesUi,
    action: &mut Option<RolesAction>,
) {
    egui::Frame::new()
        .fill(palette().WARNING_SURFACE)
        .corner_radius(egui::CornerRadius::same(R_MD))
        .inner_margin(egui::Margin::same(SP_2 as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.colored_label(
                palette().WARNING_FG,
                format!(
                    "{} Undefined (no route): route {route} does not exist, so {} fails when it runs.",
                    icons::WARNING,
                    role.label
                ),
            );
            ui.horizontal_wrapped(|ui| {
                let create = ui.add_enabled(
                    !route.trim().is_empty(),
                    egui::Button::new(icons::with_icon(icons::PLUS, format!("Create route \"{route}\""))),
                );
                accessible(&create, "Create route");
                if create.clicked() {
                    *action = Some(RolesAction::CreateRoute(route.to_owned()));
                }
                let choose = ui.add(egui::Button::new(icons::with_icon(
                    icons::PATH,
                    "Choose an existing route",
                )));
                accessible(&choose, "Choose an existing route");
                if choose.clicked() {
                    state.focus_head = true;
                }
            });
            ui.label(muted(
                "Create route opens Routing in this window with the route filled in; saving it brings you back here.",
            ));
        });
}

/// The routes and wording one chain editor offers.
#[derive(Clone, Copy)]
struct Chain<'a> {
    name: &'a str,
    routes: &'a ChainRoutes,
    requirement: &'a str,
    inherit_label: &'a str,
    focus_head: bool,
}

/// One chain: each entry on a row with its route, its effort and the arrows and trash that
/// reorder and remove it, and under it what it resolves to. The head may name a route that
/// does not exist yet; the fallbacks pick from the ones that do. With two or more entries
/// the chain cannot be emptied (inherited), so it is not deleted by accident.
fn chain_editor(
    ui: &mut egui::Ui,
    models: &mut Vec<ChainEntryConfig>,
    chain: &Chain<'_>,
    context: &Context,
) {
    let Chain {
        name,
        routes,
        requirement,
        inherit_label,
        focus_head,
    } = *chain;
    let count = models.len();
    let mut head = models.first().map(|entry| entry.route.clone());
    let can_inherit = count <= 1;
    ui.horizontal(|ui| {
        ui.label(muted("1"));
        let combo = egui::ComboBox::from_id_salt((name, "head"))
            .width(190.0)
            .selected_text(head.as_deref().unwrap_or(inherit_label))
            .show_ui(ui, |ui| {
                if can_inherit {
                    ui.selectable_value(&mut head, None, inherit_label);
                }
                for route in &routes.heads {
                    ui.selectable_value(&mut head, Some(route.clone()), route);
                }
            });
        named_combo(&combo.response, name);
        if focus_head {
            combo.response.request_focus();
        }
        if let Some(entry) = models.first_mut()
            && let Some(route) = &head
        {
            entry.route.clone_from(route);
        }
        if let Some(entry) = models.first_mut() {
            entry_effort(ui, entry, context, name);
        }
    });
    match (head, models.first()) {
        (None, _) => models.clear(),
        (Some(route), None) => models.push(ChainEntryConfig::route(route)),
        (Some(_), Some(_)) => {}
    }
    resolved_line(ui, models.first(), context);
    let mut movement = None;
    let mut removal = None;
    let count = models.len();
    if count > 0 {
        arrows(ui, name, 0, count, &mut movement, &mut removal);
    }
    for (index, entry) in models.iter_mut().enumerate().skip(1) {
        let entry_name = format!("{name} fallback {index}");
        ui.push_id(&entry_name, |ui| {
            ui.horizontal(|ui| {
                ui.label(muted((index + 1).to_string()));
                let combo = egui::ComboBox::from_id_salt(&entry_name)
                    .width(190.0)
                    .selected_text(entry.route.as_str())
                    .show_ui(ui, |ui| {
                        for route in &routes.existing {
                            ui.selectable_value(&mut entry.route, route.clone(), route);
                        }
                    });
                named_combo(&combo.response, &entry_name);
                entry_effort(ui, entry, context, &entry_name);
            });
            resolved_line(ui, Some(entry), context);
        });
        arrows(ui, &entry_name, index, count, &mut movement, &mut removal);
    }
    if let Some((from, to)) = movement {
        models.swap(from, to);
    } else if let Some(index) = removal {
        models.remove(index);
    }
    for entry in models
        .iter()
        .filter(|entry| routes.misfits.contains(&entry.route))
    {
        ui.colored_label(
            palette().ERROR_FG,
            format!(
                "Route `{}` has models of another kind. {requirement}",
                entry.route
            ),
        );
    }
    let next = routes
        .existing
        .iter()
        .find(|route| !models.iter().any(|entry| &entry.route == *route));
    let add = ui.add_enabled(
        !models.is_empty() && next.is_some(),
        ghost(icons::with_icon(icons::PLUS, "Add fallback")),
    );
    accessible(&add, &format!("Add {name} fallback"));
    if add
        .on_disabled_hover_text("Choose the first route, and define another route to fall back to")
        .clicked()
        && let Some(route) = next
    {
        models.push(ChainEntryConfig::route(route.clone()));
    }
}

/// The ↑ ↓ and trash of one entry, on a line of their own under its resolved target so the
/// row itself stays short.
fn arrows(
    ui: &mut egui::Ui,
    name: &str,
    index: usize,
    count: usize,
    movement: &mut Option<(usize, usize)>,
    removal: &mut Option<usize>,
) {
    ui.horizontal(|ui| {
        ui.add_space(SP_4);
        ui.add_enabled_ui(index > 0, |ui| {
            if icon_button(ui, icons::ARROW_UP, &format!("Move {name} up")).clicked() {
                *movement = Some((index, index - 1));
            }
        });
        ui.add_enabled_ui(index + 1 < count, |ui| {
            if icon_button(ui, icons::ARROW_DOWN, &format!("Move {name} down")).clicked() {
                *movement = Some((index, index + 1));
            }
        });
        // The only entry is removed by choosing to inherit, not here.
        if (index > 0 || count > 1)
            && icon_button(ui, icons::TRASH, &format!("Remove {name}")).clicked()
        {
            *removal = Some(index);
        }
    });
}

fn resolved_line(ui: &mut egui::Ui, entry: Option<&ChainEntryConfig>, context: &Context) {
    let Some(entry) = entry else {
        return;
    };
    ui.horizontal(|ui| {
        ui.add_space(SP_4);
        match context.targets.get(&entry.route) {
            Some(target) => {
                ui.label(muted(format!("→ {target}")));
            }
            None if context.route_names.contains(&entry.route) => {
                ui.label(muted("→ the route has no candidates"));
            }
            None => {
                ui.colored_label(palette().WARNING_FG, "→ no route");
            }
        }
    });
}

fn entry_effort(ui: &mut egui::Ui, entry: &mut ChainEntryConfig, context: &Context, name: &str) {
    let choices = context
        .efforts
        .get(&entry.route)
        .map_or(&[][..], Vec::as_slice);
    if !choices.is_empty() || entry.reasoning_effort.is_some() {
        let response = effort_picker(
            ui,
            (name, "reasoning-effort"),
            "(provider default)",
            &mut entry.reasoning_effort,
            choices,
        );
        named_combo(&response, &format!("{name} reasoning effort"));
    }
}

/// The category overrides of the Worker or Reviewer: a row each, saying what it overrides or
/// that it inherits, and the one being edited open under its row.
fn category_overrides(
    ui: &mut egui::Ui,
    role: &RoleInfo,
    base: &RoleBindingConfig,
    categories: &mut BTreeMap<String, CategoryBindingConfig>,
    state: &mut RolesUi,
    context: &Context,
    action: &mut Option<RolesAction>,
) {
    let ids: Vec<&'static str> = categories_for_role(role.key)
        .map(|category| category.id.as_str())
        .collect();
    let set = ids
        .iter()
        .filter(|id| {
            categories
                .get(**id)
                .is_some_and(|binding| *binding != Default::default())
        })
        .count();
    ui.add_space(SP_2);
    ui.horizontal(|ui| {
        block_title(ui, "Category overrides");
        ui.label(muted(format!("{set} of {} set", ids.len())));
    });
    for id in ids {
        let binding = categories.get(id).cloned().unwrap_or_default();
        let overridden = binding != CategoryBindingConfig::default();
        let editing = state.editing.as_deref() == Some(id);
        let mut reset = false;
        let mut edit = false;
        let response = click_row(
            ui,
            ROW_DENSE,
            editing,
            |ui| {
                role_column(ui, ROW_DENSE, medium(id).color(palette().TEXT));
                if overridden {
                    override_summary(ui, &binding, context);
                } else {
                    ui.label(muted(format!("Inherits {}", role.label)));
                }
            },
            |ui| {
                edit =
                    icon_button(ui, icons::PENCIL_SIMPLE, &format!("Edit category {id}")).clicked();
                if overridden {
                    reset = icon_button(
                        ui,
                        icons::ARROW_COUNTER_CLOCKWISE,
                        &format!("Reset category {id}"),
                    )
                    .clicked();
                }
            },
        );
        named_row(&response, &format!("Category {id}"));
        if edit || response.clicked() {
            state.editing = if editing { None } else { Some(id.to_owned()) };
        }
        if reset {
            categories.remove(id);
        }
        if state.editing.as_deref() == Some(id) && !reset {
            category_editor(ui, role, base, categories, id, context, action);
        }
    }
}

fn override_summary(ui: &mut egui::Ui, binding: &CategoryBindingConfig, context: &Context) {
    if let Some(entry) = binding.models.first() {
        route_chip(ui, &entry.route);
        let missing = !context.route_names.contains(&entry.route);
        if missing {
            warning_chip(ui, format!("{} No route", icons::WARNING));
        } else if let Some(effort) = &entry.reasoning_effort {
            ui.label(muted(effort));
        }
    }
    if let Some(preset) = &binding.preset {
        ui.label(muted(format!("preset {preset}")));
    }
    let mut tuning = Vec::new();
    if let Some(temperature) = binding.generation.temperature {
        tuning.push(format!("temp {temperature}"));
    }
    if let Some(tokens) = binding.generation.max_tokens {
        tuning.push(format!("max {tokens}"));
    }
    if !tuning.is_empty() {
        ui.label(muted(tuning.join(" · ")));
    }
}

/// Edits one category override in place, under its row, where it stays beside the role it
/// overrides. An override that has been emptied is removed again.
fn category_editor(
    ui: &mut egui::Ui,
    role: &RoleInfo,
    base: &RoleBindingConfig,
    categories: &mut BTreeMap<String, CategoryBindingConfig>,
    id: &str,
    context: &Context,
    action: &mut Option<RolesAction>,
) {
    let mut draft = categories.get(id).cloned().unwrap_or_default();
    egui::Frame::new()
        .fill(palette().SURFACE)
        .stroke(egui::Stroke::new(1.0, palette().BORDER))
        .corner_radius(egui::CornerRadius::same(R_MD))
        .inner_margin(egui::Margin::same(SP_2 as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.push_id(("category", id), |ui| {
                let name = format!("{id} route");
                chain_editor(
                    ui,
                    &mut draft.models,
                    &Chain {
                        name: &name,
                        routes: &context.conversation,
                        requirement: CHAT_ROUTES,
                        inherit_label: &format!("Inherit {}", role.label),
                        focus_head: false,
                    },
                    context,
                );
                let effective = draft
                    .head_route()
                    .or_else(|| base.head_route())
                    .unwrap_or(role.key);
                if !context.route_names.contains(effective) {
                    ui.horizontal_wrapped(|ui| {
                        warning_chip(ui, "Undefined (no route)");
                        if ui.button("Create route").clicked() {
                            *action = Some(RolesAction::CreateRoute(effective.to_owned()));
                        }
                    });
                }
                optional_text(ui, "Category preset reference", &mut draft.preset);
                generation(ui, &mut draft.generation);
            });
        });
    if draft == CategoryBindingConfig::default() {
        categories.remove(id);
    } else {
        categories.insert(id.to_owned(), draft);
    }
}

/// The preset reference and the generation overrides of a role, folded away because most
/// roles use neither; the closed header says what is set.
fn advanced(ui: &mut egui::Ui, role: &RoleInfo, binding: &mut RoleBindingConfig) {
    let mut set = Vec::new();
    if let Some(preset) = &binding.preset {
        set.push(format!("preset {preset}"));
    }
    if let Some(temperature) = binding.generation.temperature {
        set.push(format!("temp {temperature}"));
    }
    if let Some(tokens) = binding.generation.max_tokens {
        set.push(format!("max {tokens}"));
    }
    let summary = if set.is_empty() {
        "Preset · Temperature · Max tokens".to_owned()
    } else {
        set.join(" · ")
    };
    ui.add_space(SP_2);
    // A compactor without a chain follows the running role, so it has nothing to tune.
    let editable = !(role.is_compactor() && binding.models.is_empty());
    ui.add_enabled_ui(editable, |ui| {
        egui::CollapsingHeader::new(crate::theme::text::section(format!("ADVANCED  {summary}")))
            .id_salt(("advanced", role.address))
            .default_open(false)
            .show(ui, |ui| {
                if !role.is_compactor() {
                    optional_text(ui, "Preset reference", &mut binding.preset);
                }
                generation(ui, &mut binding.generation);
            });
    });
}
