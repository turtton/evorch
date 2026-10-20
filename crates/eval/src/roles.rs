//! Which roles a candidate overlay swaps, which models a role's chain resolves
//! to, and the model families compared for the judge's self-preference warning
//! (ADR 0036 §9).

use std::collections::BTreeMap;

use catalog::ModelCatalog;
use config::{ChainEntryConfig, Config, ModelEntryConfig, ProviderTypeConfig};
use serde::{Deserialize, Serialize};

/// Every role address below `[agents]` a candidate can swap.
pub const ROLES: [&str; 10] = [
    "orchestrator",
    "explorer",
    "worker",
    "reviewer",
    "roles.web_researcher",
    "roles.planner",
    "roles.advisor",
    "roles.multimodal_looker",
    "roles.evaluator",
    "roles.compactor",
];

/// One concrete model a role's chain can send to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_type: Option<ProviderTypeConfig>,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwappedRole {
    /// The role's address below `[agents]`, such as `worker` or `roles.planner`.
    pub role: String,
    pub baseline: Vec<ModelRef>,
    pub candidate: Vec<ModelRef>,
}

/// The address of a role given by its address (`roles.planner`), its key
/// (`planner`) or its name (`Planner`, `WebResearcher`).
pub fn address(role: &str) -> Option<&'static str> {
    let key = |name: &str| {
        name.trim_start_matches("roles.")
            .replace('_', "")
            .to_ascii_lowercase()
    };
    let wanted = key(role.trim());
    ROLES.into_iter().find(|address| key(address) == wanted)
}

/// Roles whose binding, categories, referenced routes or presets differ.
pub fn swapped_roles(baseline: &Config, candidate: &Config) -> Vec<SwappedRole> {
    ROLES
        .iter()
        .filter(|role| fingerprint(baseline, role) != fingerprint(candidate, role))
        .map(|role| SwappedRole {
            role: (*role).to_owned(),
            baseline: role_models(baseline, role),
            candidate: role_models(candidate, role),
        })
        .collect()
}

struct RoleChains {
    binding: serde_json::Value,
    routes: Vec<String>,
    presets: Vec<String>,
}

fn chains(config: &Config, address: &str) -> RoleChains {
    let agents = &config.agents;
    let (binding, base, categories) = match address {
        "orchestrator" => (to_json(&agents.orchestrator), &agents.orchestrator, None),
        "explorer" => (to_json(&agents.explorer), &agents.explorer, None),
        "worker" => (
            to_json(&agents.worker),
            &agents.worker.base,
            Some(&agents.worker.categories),
        ),
        "reviewer" => (
            to_json(&agents.reviewer),
            &agents.reviewer.base,
            Some(&agents.reviewer.categories),
        ),
        "roles.web_researcher" => (
            to_json(&agents.roles.web_researcher),
            &agents.roles.web_researcher,
            None,
        ),
        "roles.planner" => (to_json(&agents.roles.planner), &agents.roles.planner, None),
        "roles.advisor" => (to_json(&agents.roles.advisor), &agents.roles.advisor, None),
        "roles.multimodal_looker" => (
            to_json(&agents.roles.multimodal_looker),
            &agents.roles.multimodal_looker,
            None,
        ),
        "roles.evaluator" => (
            to_json(&agents.roles.evaluator),
            &agents.roles.evaluator,
            None,
        ),
        _ => (
            to_json(&agents.roles.compactor),
            &agents.roles.compactor,
            None,
        ),
    };
    // Without a chain a role uses the route named after it; the compactor uses the run's own.
    let implicit = (address != "roles.compactor")
        .then(|| ChainEntryConfig::route(address.trim_start_matches("roles.")));
    let mut routes: Vec<String> = if base.models.is_empty() {
        implicit.into_iter().map(|entry| entry.route).collect()
    } else {
        base.models
            .iter()
            .map(|entry| entry.route.clone())
            .collect()
    };
    let mut presets: Vec<String> = base.preset.iter().cloned().collect();
    for category in categories.into_iter().flat_map(BTreeMap::values) {
        routes.extend(category.models.iter().map(|entry| entry.route.clone()));
        presets.extend(category.preset.iter().cloned());
    }
    routes.sort();
    routes.dedup();
    presets.sort();
    presets.dedup();
    RoleChains {
        binding,
        routes,
        presets,
    }
}

fn to_json(value: &impl Serialize) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or_default()
}

fn fingerprint(config: &Config, address: &str) -> serde_json::Value {
    let chains = chains(config, address);
    let routes: BTreeMap<_, _> = chains
        .routes
        .iter()
        .map(|route| (route, to_json(&config.routing.routes.get(route))))
        .collect();
    let presets: BTreeMap<_, _> = chains
        .presets
        .iter()
        .map(|preset| (preset, to_json(&config.model_presets.get(preset))))
        .collect();
    serde_json::json!({
        "binding": chains.binding,
        "routes": routes,
        "presets": presets,
        "models": role_models(config, address),
    })
}

/// Every model the role's chains name, through their routes and profiles.
pub fn role_models(config: &Config, address: &str) -> Vec<ModelRef> {
    let mut models: Vec<ModelRef> = chains(config, address)
        .routes
        .iter()
        .filter_map(|route| config.routing.routes.get(route))
        .flatten()
        .map(|candidate| {
            let profile = config.providers.get(&candidate.profile);
            ModelRef {
                profile: candidate.profile.clone(),
                provider_type: profile.map(|profile| profile.provider_type),
                model: candidate
                    .model
                    .clone()
                    .or_else(|| profile.map(|profile| profile.default_model.clone()))
                    .unwrap_or_default(),
            }
        })
        .collect();
    models.sort_by(|left, right| (&left.profile, &left.model).cmp(&(&right.profile, &right.model)));
    models.dedup();
    models
}

/// models.dev's family when the catalog knows the model, otherwise the
/// lowercased model name without any `provider/` prefix.
pub fn family(model: &ModelRef, catalog: Option<&ModelCatalog>) -> String {
    catalog
        .and_then(|catalog| {
            runtime::model_resolve::resolve_catalog_entry(
                &ModelEntryConfig::enabled(&model.model),
                catalog,
                Some(&model.profile),
                model.provider_type,
            )
        })
        .and_then(|metadata| metadata.family.clone())
        .unwrap_or_else(|| {
            let name = model.model.rsplit('/').next().unwrap_or(&model.model);
            name.to_ascii_lowercase()
        })
}

/// Families are compared by their first `-` separated word, the vendor's model
/// line, so `claude-sonnet` and `claude-opus` share one.
pub fn family_line(family: &str) -> &str {
    family
        .split(|c: char| !c.is_ascii_alphanumeric())
        .find(|word| !word.is_empty())
        .unwrap_or(family)
}

#[cfg(test)]
mod tests {
    use config::{ProviderProfileConfig, RoleBindingConfig, RouteCandidateConfig};

    use super::*;

    fn config(worker_model: &str) -> Config {
        let mut config = Config::default();
        config.providers.insert(
            "claude".into(),
            ProviderProfileConfig {
                provider_type: ProviderTypeConfig::AnthropicSubscription,
                default_model: "claude-sonnet-9".into(),
                ..ProviderProfileConfig::default()
            },
        );
        config.routing.routes.insert(
            "orchestrator".into(),
            vec![RouteCandidateConfig {
                profile: "claude".into(),
                model: None,
                auto: false,
            }],
        );
        config.routing.routes.insert(
            "fast".into(),
            vec![RouteCandidateConfig {
                profile: "claude".into(),
                model: Some(worker_model.into()),
                auto: false,
            }],
        );
        config.agents.worker.base = RoleBindingConfig {
            models: vec![ChainEntryConfig::route("fast")],
            ..RoleBindingConfig::default()
        };
        config
    }

    #[test]
    fn only_roles_whose_resolved_chain_changes_are_swapped() {
        // Given: a candidate whose worker route points at another model.
        let baseline = config("claude-haiku-9");
        let candidate = config("gpt-9");

        // When: the swapped roles are derived.
        let swapped = swapped_roles(&baseline, &candidate);

        // Then: only the worker is swapped, with the models of both sides.
        let [worker] = swapped.as_slice() else {
            panic!("one swapped role: {swapped:?}");
        };
        assert_eq!(worker.role, "worker");
        assert_eq!(worker.baseline[0].model, "claude-haiku-9");
        assert_eq!(worker.candidate[0].model, "gpt-9");
        assert_eq!(
            role_models(&baseline, "orchestrator")[0].model,
            "claude-sonnet-9"
        );
        assert!(swapped_roles(&baseline, &baseline).is_empty());
    }

    #[test]
    fn families_fall_back_to_the_model_name_and_compare_by_line() {
        // Given: models the catalog does not know.
        let model = |name: &str| ModelRef {
            profile: "p".into(),
            provider_type: None,
            model: name.into(),
        };

        // When / Then: the name's line is compared, ignoring provider prefixes.
        assert_eq!(
            family_line(&family(&model("anthropic/Claude-Opus-9"), None)),
            "claude"
        );
        assert_eq!(family_line("claude-sonnet"), "claude");
        assert_eq!(family_line(&family(&model("gpt-9.1-codex"), None)), "gpt");
    }
}
