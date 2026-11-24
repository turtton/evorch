//! The Routing and Agent roles drafts after a save, shared by the legacy settings modal and
//! the core client's.

use std::collections::{BTreeMap, BTreeSet};

use super::role_profiles::RoleProfilePicker;
use super::role_settings::RoleSettingsModel;
use super::routing_settings::RoutingSettingsModel;

pub const ROUTING_CONFLICT: &str = "Routing was changed by another save while this draft had unsaved edits. \
     Saving is disabled so the newer routes are not overwritten; close settings \
     and open them again to start from the saved routing.";

/// Routing after another save: a clean draft is reseeded from `view`; a dirty one only learns
/// the references, and cannot be saved over routes that changed under it.
pub fn refresh_routing_after_save(
    model: &mut RoutingSettingsModel,
    view: &config::Config,
    picker: RoleProfilePicker,
) {
    if model.is_dirty() {
        let unchanged = view.routing.routes == model.base_routes;
        model.refresh_references(view);
        if !unchanged {
            model.conflict = Some(ROUTING_CONFLICT.to_owned());
        }
    } else {
        reseed_routing(model, view, picker);
    }
}

pub fn reseed_routing(
    model: &mut RoutingSettingsModel,
    view: &config::Config,
    picker: RoleProfilePicker,
) {
    let origin = model.origin_role_settings;
    *model = RoutingSettingsModel::seed_from_config(view);
    model.profiles = picker;
    model.origin_role_settings = origin;
}

pub fn expanded_after_save(model: &RoutingSettingsModel) -> BTreeSet<String> {
    model
        .expanded
        .iter()
        .map(|name| model.route_name_edits.get(name).unwrap_or(name).clone())
        .collect()
}

/// Routing was written: the routing draft becomes the saved state and the roles draft learns
/// the new routes and renames, its edits otherwise untouched.
pub fn carry_routing_save(
    routing: &mut RoutingSettingsModel,
    roles: Option<&mut RoleSettingsModel>,
    view: &config::Config,
    picker: RoleProfilePicker,
    renames: &BTreeMap<String, String>,
    expanded: BTreeSet<String>,
) {
    *routing = RoutingSettingsModel::seed_from_config(view);
    routing.profiles = picker;
    routing.expanded = expanded;
    if let Some(roles) = roles {
        if !renames.is_empty() {
            config::types::agents::rename_route_refs(&mut roles.agents, renames);
            config::types::agents::rename_route_refs(&mut roles.base_agents, renames);
        }
        roles.refresh_references(view);
    }
}
