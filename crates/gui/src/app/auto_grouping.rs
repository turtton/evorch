//! Automatic pool membership when a provider is saved, and its follow-up prompts
//! (ADR 0030).

use std::collections::BTreeSet;
use std::path::Path;

use runtime::model_grouping::{
    AutoAddition, AutoConflict, StaleAutoCandidate, apply_additions, plan_for_profile,
    stale_auto_candidates,
};

use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AutoGroupOutcome {
    /// `(role profile, addition)` written to the save target.
    pub added: Vec<(String, AutoAddition)>,
    pub conflicts: Vec<ScopedConflict>,
    /// Additions left out because another config layer defines the route.
    pub blocked: Vec<(String, AutoAddition)>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ScopedConflict {
    pub scope: String,
    pub conflict: AutoConflict,
}

/// Prompts still waiting for the user, and the ones already answered this session.
#[derive(Debug, Default)]
pub struct AutoGroupingState {
    pub conflicts: Vec<ScopedConflict>,
    pub dismissed: BTreeSet<ScopedConflict>,
    pub warned: BTreeSet<(String, StaleAutoCandidate)>,
}

fn scopes(config: &config::Config) -> Vec<(String, &config::RoutingConfig, &config::AgentsConfig)> {
    std::iter::once((
        config::DEFAULT_ROLE_PROFILE.to_owned(),
        &config.routing,
        &config.agents,
    ))
    .chain(
        config
            .role_profiles
            .iter()
            .map(|(name, profile)| (name.clone(), &profile.routing, &profile.agents)),
    )
    .collect()
}

/// Plans and writes the automatic additions for the saved `profile`.
pub(crate) fn group_saved_provider(
    path: &Path,
    options: &config::LoadOptions,
    profile: &str,
    catalog: &catalog::ModelCatalog,
) -> Result<AutoGroupOutcome, String> {
    let merged = config::Config::load_unresolved(options).map_err(|error| error.to_string())?;
    let mut outcome = AutoGroupOutcome::default();
    for (scope, routing, agents) in scopes(&merged) {
        let plan = plan_for_profile(profile, &merged.providers, routing, agents, catalog);
        outcome
            .conflicts
            .extend(plan.conflicts.into_iter().map(|conflict| ScopedConflict {
                scope: scope.clone(),
                conflict,
            }));
        if plan.additions.is_empty() {
            continue;
        }
        let (added, blocked) = write_additions(path, &merged, &scope, &plan.additions)?;
        outcome
            .added
            .extend(added.into_iter().map(|addition| (scope.clone(), addition)));
        outcome.blocked.extend(
            blocked
                .into_iter()
                .map(|addition| (scope.clone(), addition)),
        );
    }
    Ok(outcome)
}

/// Appends `additions` to the routes the save target defines in `scope`.
///
/// Only a route whose candidates in the target file equal the merged ones is
/// changed, so a route that another layer (config.d, `EVORCH_*`, CLI) defines or
/// overrides is reported as blocked instead of being copied into the target.
pub(crate) fn write_additions(
    path: &Path,
    merged: &config::Config,
    scope: &str,
    additions: &[AutoAddition],
) -> Result<(Vec<AutoAddition>, Vec<AutoAddition>), String> {
    let merged_routing = scopes(merged)
        .into_iter()
        .find(|(name, _, _)| name == scope)
        .map(|(_, routing, _)| routing.clone())
        .ok_or_else(|| format!("Unknown role profile: {scope}"))?;
    let mut file_routing = file_routing(path, scope)?;
    let (writable, blocked): (Vec<_>, Vec<_>) = additions.iter().cloned().partition(|addition| {
        file_routing.routes.contains_key(&addition.route)
            && file_routing.routes.get(&addition.route)
                == merged_routing.routes.get(&addition.route)
    });
    let added = apply_additions(&mut file_routing, &merged.providers, &writable);
    if !added.is_empty() {
        config::save_role_profile_bindings(path, Some(scope), Some(&file_routing), None)
            .map_err(|error| error.to_string())?;
    }
    Ok((added, blocked))
}

/// The routing table of `scope` as written in `path` alone.
fn file_routing(path: &Path, scope: &str) -> Result<config::RoutingConfig, String> {
    let root = match std::fs::read_to_string(path) {
        Ok(content) => toml::from_str::<toml::Table>(&content).map_err(|e| e.to_string())?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(error) => return Err(error.to_string()),
    };
    let table = if scope == config::DEFAULT_ROLE_PROFILE {
        Some(&root)
    } else {
        root.get("role_profiles")
            .and_then(|profiles| profiles.get(scope))
            .and_then(toml::Value::as_table)
    };
    table
        .and_then(|table| table.get("routing"))
        .cloned()
        .map(toml::Value::try_into)
        .transpose()
        .map_err(|error| error.to_string())
        .map(Option::unwrap_or_default)
}

impl<S: AgentRunSource> WorkbenchState<S> {
    pub fn auto_group_conflicts(&self) -> &[ScopedConflict] {
        &self.auto_grouping.conflicts
    }

    /// Reports what a provider save added automatically and queues its conflicts.
    pub(super) fn apply_auto_group_outcome(&mut self, outcome: AutoGroupOutcome) {
        for (scope, addition) in &outcome.added {
            self.push_notice(format!(
                "Added {}/{} to route {}{} as an automatic fallback path",
                addition.candidate.profile,
                addition.candidate.model.as_deref().unwrap_or_default(),
                addition.route,
                scope_suffix(scope),
            ));
        }
        for (scope, addition) in &outcome.blocked {
            self.push_notice(format!(
                "Route {}{} is defined in another config layer; {}/{} was not added",
                addition.route,
                scope_suffix(scope),
                addition.candidate.profile,
                addition.candidate.model.as_deref().unwrap_or_default(),
            ));
        }
        for conflict in outcome.conflicts {
            if !self.auto_grouping.dismissed.contains(&conflict)
                && !self.auto_grouping.conflicts.contains(&conflict)
            {
                self.auto_grouping.conflicts.push(conflict);
            }
        }
    }

    /// Adds the chosen model of the first queued conflict, or skips it with `None`.
    pub fn resolve_auto_group_conflict(&mut self, model: Option<String>) {
        if self.settings_save_in_progress() || self.auto_grouping.conflicts.is_empty() {
            return;
        }
        let conflict = self.auto_grouping.conflicts.remove(0);
        self.auto_grouping.dismissed.insert(conflict.clone());
        let (Some(model), Some(path)) = (model, self.provider_settings_path.clone()) else {
            return;
        };
        let options = self.routing_load_options();
        let addition = AutoAddition {
            route: conflict.conflict.route.clone(),
            candidate: config::RouteCandidateConfig {
                profile: conflict.conflict.profile.clone(),
                model: Some(model),
                auto: true,
            },
        };
        self.provider_job(move || {
            let merged =
                config::Config::load_unresolved(&options).map_err(|error| error.to_string())?;
            let (added, blocked) = write_additions(&path, &merged, &conflict.scope, &[addition])?;
            Ok(AutoGroupOutcome {
                added: added
                    .into_iter()
                    .map(|a| (conflict.scope.clone(), a))
                    .collect(),
                blocked: blocked
                    .into_iter()
                    .map(|a| (conflict.scope.clone(), a))
                    .collect(),
                conflicts: Vec::new(),
            })
        });
    }

    /// Warns once per session about auto candidates whose catalog identity changed.
    /// The configuration is never rewritten.
    pub fn warn_stale_auto_candidates(&mut self) {
        let Some(catalog) = self.provider_settings.catalog.catalog.clone() else {
            return;
        };
        let Ok(config) = config::Config::load(&self.routing_load_options()) else {
            return;
        };
        let stale: Vec<_> = scopes(&config)
            .into_iter()
            .flat_map(|(scope, routing, _)| {
                stale_auto_candidates(routing, &config.providers, &catalog)
                    .into_iter()
                    .map(move |candidate| (scope.clone(), candidate))
            })
            .collect();
        for (scope, candidate) in stale {
            if self
                .auto_grouping
                .warned
                .insert((scope.clone(), candidate.clone()))
            {
                let detail = if candidate.actual.kind == candidate.expected.kind {
                    format!(
                        "models.dev now lists the automatic path {}/{} as {} instead of {}",
                        candidate.profile,
                        candidate.model,
                        candidate.actual.canonical,
                        candidate.expected.canonical,
                    )
                } else {
                    format!(
                        "the automatic path {}/{} is now a {} model, but the route serves {} models",
                        candidate.profile,
                        candidate.model,
                        candidate.actual.kind,
                        candidate.expected.kind,
                    )
                };
                self.push_notice(format!(
                    "Route {}{}: {detail}. The configuration was not changed; review the route in Routing settings.",
                    candidate.route,
                    scope_suffix(&scope),
                ));
            }
        }
    }

    pub(super) fn render_auto_group_conflicts(&mut self, ctx: &egui::Context) {
        let Some(conflict) = self.auto_grouping.conflicts.first() else {
            return;
        };
        let mut choice = None;
        egui::Modal::new(egui::Id::new("auto-group-conflict"))
            .show(ctx, |ui| {
                ui.heading("Choose a fallback path");
                ui.label(format!(
                    "{} offers several models of route {}{}. Choose the one to add as an automatic fallback path.",
                    conflict.conflict.profile,
                    conflict.conflict.route,
                    scope_suffix(&conflict.scope),
                ));
                for model in &conflict.conflict.models {
                    if ui.button(format!("Add {model}")).clicked() {
                        choice = Some(Some(model.clone()));
                    }
                }
                if ui.button("Skip").clicked() {
                    choice = Some(None);
                }
            });
        if let Some(model) = choice {
            self.resolve_auto_group_conflict(model);
        }
    }
}

fn scope_suffix(scope: &str) -> String {
    if scope == config::DEFAULT_ROLE_PROFILE {
        String::new()
    } else {
        format!(" (role profile {scope})")
    }
}
