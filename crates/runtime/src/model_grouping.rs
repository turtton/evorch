//! Automatic grouping of the same model across providers by models.dev's canonical id
//! (ADR 0030).
//!
//! A route is a pool of paths to one model. When a provider is saved, each of its
//! enabled models whose (canonical id, tier, kind) matches a pool that some role uses
//! is appended to that pool with `auto = true`. The result is written to the
//! configuration once; loading never recomputes it, so a later change of the
//! canonical id only produces a [`StaleAutoCandidate`] warning.

use std::collections::{BTreeMap, BTreeSet};

use catalog::{ModelCatalog, ModelMetadata};
use config::types::provider::{ModelSpeed, parse_model_speed};
use config::{
    AgentsConfig, MetadataSource, ModelEntryConfig, ModelKind, ProviderProfileConfig,
    RouteCandidateConfig, RoutingConfig,
};

/// Speed or price tier. Members of one pool share it, so a pool never mixes tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Standard,
    Fast,
    Flex,
    Eco,
    Free,
}

/// The identity a pool groups by. A pool never mixes model kinds (ADR 0038), so a chat
/// model and an image model never join the same pool even when the catalog relates them.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct GroupKey {
    pub canonical: String,
    pub tier: Tier,
    pub kind: ModelKind,
}

/// A candidate to append at the lowest priority of `route`.
#[derive(Debug, Clone, PartialEq)]
pub struct AutoAddition {
    pub route: String,
    pub candidate: RouteCandidateConfig,
}

/// Several enabled models of one profile match `route`; the user picks one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AutoConflict {
    pub route: String,
    pub profile: String,
    pub models: Vec<String>,
}

/// What saving a provider adds automatically, and what it leaves to the user.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AutoGroupPlan {
    pub additions: Vec<AutoAddition>,
    pub conflicts: Vec<AutoConflict>,
}

/// An `auto` candidate whose catalog identity no longer matches its pool.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct StaleAutoCandidate {
    pub route: String,
    pub profile: String,
    pub model: String,
    pub expected: GroupKey,
    pub actual: GroupKey,
}

/// The tier a model ID names: evorch's `+fast`, or a provider's `-fast` / `-flex` /
/// `-eco` / `-free` / `-highspeed` segment.
pub fn tier_of(model_id: &str) -> Tier {
    let (base, speed) = parse_model_speed(model_id);
    if speed == ModelSpeed::Fast {
        return Tier::Fast;
    }
    let name = base.rsplit('/').next().unwrap_or(base).to_ascii_lowercase();
    let segments: BTreeSet<_> = name.split(['-', ':', '_', '@', '.']).collect();
    [
        (["fast", "highspeed"].as_slice(), Tier::Fast),
        (["flex"].as_slice(), Tier::Flex),
        (["eco"].as_slice(), Tier::Eco),
        (["free"].as_slice(), Tier::Free),
    ]
    .into_iter()
    .find(|(names, _)| names.iter().any(|name| segments.contains(name)))
    .map_or(Tier::Standard, |(_, tier)| tier)
}

/// The catalog identity of `model_id` served by `profile`, or `None` when the catalog
/// cannot place it. Manually described models are never matched.
pub fn group_key(
    profile_name: &str,
    profile: &ProviderProfileConfig,
    model_id: &str,
    catalog: &ModelCatalog,
) -> Option<GroupKey> {
    let (base, _) = parse_model_speed(model_id);
    let entry = profile
        .models
        .iter()
        .find(|entry| entry.id == model_id || entry.id == base)
        .cloned()
        .unwrap_or_else(|| ModelEntryConfig::enabled(base));
    let canonical = canonical_entry(&entry, base, profile_name, profile, catalog)?
        .canonical_model_id
        .clone()?;
    Some(GroupKey {
        canonical,
        tier: tier_of(model_id),
        kind: profile.model_kind(model_id),
    })
}

/// Explicit `metadata_ref`, then the provider type's models.dev slugs, then the profile
/// name as a slug, then a unique match across the catalog.
fn canonical_entry<'a>(
    entry: &ModelEntryConfig,
    base: &str,
    profile_name: &str,
    profile: &ProviderProfileConfig,
    catalog: &'a ModelCatalog,
) -> Option<&'a ModelMetadata> {
    if matches!(
        entry.metadata_source,
        Some(MetadataSource::Manual | MetadataSource::ProviderDefault)
    ) {
        return None;
    }
    if let Some(reference) = entry.metadata_ref.as_deref() {
        return reference
            .split_once('/')
            .and_then(|(provider, model)| catalog.find(provider, model))
            .or_else(|| catalog.find_unique_model(reference));
    }
    profile
        .provider_type
        .models_dev_provider_candidates()
        .iter()
        .find_map(|slug| catalog.find(slug, base))
        .or_else(|| catalog.find(profile_name, base))
        .or_else(|| catalog.find_unique_model(base))
}

/// Routes that some role or category uses, explicitly or by the role-name fallback.
pub fn referenced_routes(routing: &RoutingConfig, agents: &AgentsConfig) -> BTreeSet<String> {
    let mut routes: BTreeSet<_> = config::types::agents::explicit_refs(agents)
        .into_iter()
        .map(|(_, route)| route)
        .collect();
    routes.extend(
        routing
            .routes
            .keys()
            .filter(|route| !config::types::agents::implicit_roles_using(route, agents).is_empty())
            .cloned(),
    );
    routes
}

fn candidate_model<'a>(
    candidate: &'a RouteCandidateConfig,
    providers: &'a BTreeMap<String, ProviderProfileConfig>,
) -> Option<&'a str> {
    candidate.model.as_deref().or_else(|| {
        providers
            .get(&candidate.profile)
            .map(|profile| profile.default_model.as_str())
    })
}

/// A pool's identity from its configured (non-auto) members; `None` when any of them
/// cannot be placed or they disagree.
pub fn route_key(
    candidates: &[RouteCandidateConfig],
    providers: &BTreeMap<String, ProviderProfileConfig>,
    catalog: &ModelCatalog,
) -> Option<GroupKey> {
    let configured: Vec<_> = candidates.iter().filter(|c| !c.auto).collect();
    let members = if configured.is_empty() {
        candidates.iter().collect()
    } else {
        configured
    };
    let mut keys = members.into_iter().map(|candidate| {
        let profile = providers.get(&candidate.profile)?;
        group_key(
            &candidate.profile,
            profile,
            candidate_model(candidate, providers)?,
            catalog,
        )
    });
    let first = keys.next()??;
    keys.all(|key| key.as_ref() == Some(&first))
        .then_some(first)
}

/// Plans the automatic additions for the saved `profile` in one routing scope.
///
/// A pool is skipped when the profile already serves it, when its identity is unknown,
/// or when no role uses it. One matching model is added; several are a conflict.
pub fn plan_for_profile(
    profile_name: &str,
    providers: &BTreeMap<String, ProviderProfileConfig>,
    routing: &RoutingConfig,
    agents: &AgentsConfig,
    catalog: &ModelCatalog,
) -> AutoGroupPlan {
    let mut plan = AutoGroupPlan::default();
    let Some(profile) = providers.get(profile_name) else {
        return plan;
    };
    let models: Vec<(String, GroupKey)> = profile
        .enabled_model_ids()
        .into_iter()
        .filter_map(|id| {
            let key = group_key(profile_name, profile, &id, catalog)?;
            Some((id, key))
        })
        .collect();
    if models.is_empty() {
        return plan;
    }
    for route in referenced_routes(routing, agents) {
        let Some(candidates) = routing.routes.get(&route) else {
            continue;
        };
        if candidates.iter().any(|c| c.profile == profile_name) {
            continue;
        }
        let Some(key) = route_key(candidates, providers, catalog) else {
            continue;
        };
        let matching: Vec<_> = models
            .iter()
            .filter(|(_, model_key)| *model_key == key)
            .map(|(id, _)| id.clone())
            .collect();
        match matching.as_slice() {
            [] => {}
            [model] => plan.additions.push(AutoAddition {
                route,
                candidate: RouteCandidateConfig {
                    profile: profile_name.to_owned(),
                    model: Some(model.clone()),
                    auto: true,
                },
            }),
            _ => plan.conflicts.push(AutoConflict {
                route,
                profile: profile_name.to_owned(),
                models: matching,
            }),
        }
    }
    plan
}

/// Appends each addition at the lowest priority, skipping a (profile, model) the pool
/// already has. Returns the additions that were applied.
pub fn apply_additions(
    routing: &mut RoutingConfig,
    providers: &BTreeMap<String, ProviderProfileConfig>,
    additions: &[AutoAddition],
) -> Vec<AutoAddition> {
    let mut applied = Vec::new();
    for addition in additions {
        let Some(candidates) = routing.routes.get_mut(&addition.route) else {
            continue;
        };
        let model = candidate_model(&addition.candidate, providers);
        if candidates.iter().any(|existing| {
            existing.profile == addition.candidate.profile
                && candidate_model(existing, providers) == model
        }) {
            continue;
        }
        candidates.push(addition.candidate.clone());
        applied.push(addition.clone());
    }
    applied
}

/// `auto` candidates whose catalog identity changed since they were added. Candidates
/// the catalog cannot place now are not reported, since that is not a confirmed change.
pub fn stale_auto_candidates(
    routing: &RoutingConfig,
    providers: &BTreeMap<String, ProviderProfileConfig>,
    catalog: &ModelCatalog,
) -> Vec<StaleAutoCandidate> {
    let mut stale = Vec::new();
    for (route, candidates) in &routing.routes {
        if !candidates.iter().any(|c| !c.auto) {
            continue;
        }
        let Some(expected) = route_key(candidates, providers, catalog) else {
            continue;
        };
        for candidate in candidates.iter().filter(|c| c.auto) {
            let (Some(profile), Some(model)) = (
                providers.get(&candidate.profile),
                candidate_model(candidate, providers),
            ) else {
                continue;
            };
            if let Some(actual) = group_key(&candidate.profile, profile, model, catalog)
                && actual != expected
            {
                stale.push(StaleAutoCandidate {
                    route: route.clone(),
                    profile: candidate.profile.clone(),
                    model: model.to_owned(),
                    expected: expected.clone(),
                    actual,
                });
            }
        }
    }
    stale
}

#[cfg(test)]
mod tests;
