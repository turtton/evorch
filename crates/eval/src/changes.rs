//! What a candidate changes compared with the baseline, and whether the
//! comparison is valid, decided from the two configurations alone: no replay
//! and no provider request is needed (ADR 0036 §9).
//!
//! `swapped_roles` stays the record of role binding differences. Tool
//! description differences are recorded here, so a description-only candidate
//! is never described as a model swap.

use std::collections::{BTreeMap, BTreeSet};

use config::Config;
use runtime::Role;
use runtime::tool_descriptions::BuiltinCatalog;
use serde::{Deserialize, Serialize};
use storage::eval_sample::sha256_hex;

use crate::roles;

/// One tool's description for one exact model, baseline against candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDescriptionChange {
    pub profile: String,
    /// The route model ID the description is selected by, `+fast` included.
    pub route_model_id: String,
    pub tool: String,
    /// SHA-256 of the description each side sends: its override, or else the
    /// build's own text.
    pub baseline_hash: String,
    pub candidate_hash: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changes {
    /// Role addresses whose binding, categories, routes or presets differ;
    /// the details are the manifest's `swapped_roles`.
    #[serde(default)]
    pub bindings: Vec<String>,
    #[serde(default)]
    pub tool_descriptions: Vec<ToolDescriptionChange>,
    /// Roles the differences can reach: the swapped roles, and the roles that
    /// use a model whose description changed and can see the changed tool.
    #[serde(default)]
    pub affected_roles: Vec<String>,
}

const NO_CHANGE: &str = "the candidate changes nothing: no role binding, route or preset differs \
and no tool description it sets differs from the baseline's, so both configurations would be the \
same; with a role_profile active, change role_profiles.<name>";

/// Compare the two configurations and refuse an invalid comparison.
///
/// # Errors
/// - the candidate changes nothing the baseline does not already have
/// - it sets a description for a tool that is not a fixed builtin (the
///   `run_js` description, team, MCP and host tools cannot be changed)
/// - it changes a role's model together with a description, so a difference
///   could not be told apart
/// - a changed description reaches no role: no role uses that model or can
///   see that tool
/// - a changed description is never sent to that model: its edit interface or
///   wire does not carry it (see [`description_delivered`])
pub fn derive(
    baseline: &Config,
    candidate: &Config,
    catalog: &BuiltinCatalog,
) -> Result<Changes, String> {
    let swapped = roles::swapped_roles(baseline, candidate);
    let descriptions = description_changes(baseline, candidate, &catalog.descriptions())?;
    if swapped.is_empty() && descriptions.is_empty() {
        return Err(NO_CHANGE.into());
    }
    let mut affected: BTreeSet<String> = swapped.iter().map(|role| role.role.clone()).collect();
    if !descriptions.is_empty() {
        if let Some(role) = swapped.iter().find(|role| {
            roles::model_settings(baseline, &role.role)
                != roles::model_settings(candidate, &role.role)
        }) {
            return Err(format!(
                "the candidate changes the model settings of {} together with tool descriptions; \
                 compare one at a time, so a difference has one cause",
                role.role
            ));
        }
        for change in &descriptions {
            let reached = reached_roles(candidate, catalog, change);
            if reached.is_empty() {
                if !description_delivered(
                    candidate,
                    catalog,
                    &change.profile,
                    &change.route_model_id,
                    &change.tool,
                ) {
                    return Err(format!(
                        "the description of `{}` for {}/{} is never sent to that model: its edit \
                         interface or wire does not carry it (`edit` and `write` are sent only \
                         with the edit interface, and an `apply_patch` description is replaced on \
                         the Responses wire, so it cannot be confirmed to arrive)",
                        change.tool, change.profile, change.route_model_id
                    ));
                }
                return Err(format!(
                    "the description of `{}` for {}/{} reaches no role: no role's chain uses \
                     that model, or none of them can see that tool",
                    change.tool, change.profile, change.route_model_id
                ));
            }
            affected.extend(reached);
        }
    }
    Ok(Changes {
        bindings: swapped.into_iter().map(|role| role.role).collect(),
        tool_descriptions: descriptions,
        affected_roles: affected.into_iter().collect(),
    })
}

fn description_changes(
    baseline: &Config,
    candidate: &Config,
    defaults: &BTreeMap<String, String>,
) -> Result<Vec<ToolDescriptionChange>, String> {
    let overrides = |config: &Config| -> BTreeMap<(String, String, String), String> {
        config
            .model_tool_descriptions
            .iter()
            .flat_map(|(profile, models)| {
                models.iter().flat_map(move |(model, tools)| {
                    tools.iter().map(move |(tool, text)| {
                        ((profile.clone(), model.clone(), tool.clone()), text.clone())
                    })
                })
            })
            .collect()
    };
    let (before, after) = (overrides(baseline), overrides(candidate));
    let keys: BTreeSet<_> = before.keys().chain(after.keys()).collect();
    let mut changes = Vec::new();
    for key in keys {
        let (old, new) = (before.get(key), after.get(key));
        if old == new {
            continue;
        }
        let (profile, model, tool) = key;
        let Some(default) = defaults.get(tool) else {
            return Err(format!(
                "`{tool}` ({profile}/{model}) is not a fixed builtin tool, so its description \
                 cannot be changed; the run_js description, team tools, MCP tools and host \
                 tools are not accepted"
            ));
        };
        let hash = |text: Option<&String>| sha256_hex(text.unwrap_or(default).as_bytes());
        let (baseline_hash, candidate_hash) = (hash(old), hash(new));
        if baseline_hash == candidate_hash {
            continue;
        }
        changes.push(ToolDescriptionChange {
            profile: profile.clone(),
            route_model_id: model.clone(),
            tool: tool.clone(),
            baseline_hash,
            candidate_hash,
        });
    }
    Ok(changes)
}

const DESCRIBABLE_ROLES: [Role; 9] = [
    Role::Orchestrator,
    Role::Explorer,
    Role::Worker,
    Role::Reviewer,
    Role::WebResearcher,
    Role::Planner,
    Role::Advisor,
    Role::MultimodalLooker,
    Role::Evaluator,
];

fn runtime_role(address: &str) -> Option<Role> {
    let key = address.trim_start_matches("roles.").replace('_', "");
    DESCRIBABLE_ROLES
        .into_iter()
        .find(|role| role.name().eq_ignore_ascii_case(&key))
}

/// Whether the description of `tool` for `profile`/`model` is sent to the
/// model at all, by the edit interface `config` gives that model. An unknown
/// profile delivers nothing.
pub(crate) fn description_delivered(
    config: &Config,
    catalog: &BuiltinCatalog,
    profile: &str,
    model: &str,
    tool: &str,
) -> bool {
    config.providers.get(profile).is_some_and(|provider| {
        catalog.description_reaches_wire(tool, provider.edit_tool_for(model))
    })
}

/// The role addresses whose chains use the changed description's exact model,
/// that can be offered the tool and to whose wire the description is sent.
fn reached_roles(
    config: &Config,
    catalog: &BuiltinCatalog,
    change: &ToolDescriptionChange,
) -> Vec<String> {
    roles::ROLES
        .iter()
        .filter(|address| {
            description_delivered(
                config,
                catalog,
                &change.profile,
                &change.route_model_id,
                &change.tool,
            ) && runtime_role(address).is_some_and(|role| catalog.visible_to(role, &change.tool))
                && roles::role_models(config, address).iter().any(|model| {
                    model.profile == change.profile && model.model == change.route_model_id
                })
        })
        .map(|address| (*address).to_owned())
        .collect()
}

#[cfg(test)]
mod tests;
