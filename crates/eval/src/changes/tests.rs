use config::RouteCandidateConfig;
use config::{ChainEntryConfig, ProviderProfileConfig, ProviderTypeConfig, RoleBindingConfig};

use super::*;

fn route(model: &str) -> Vec<RouteCandidateConfig> {
    vec![RouteCandidateConfig {
        profile: "p".into(),
        model: Some(model.into()),
        auto: false,
    }]
}

/// The worker talks to `m`, the explorer to `m3`.
fn baseline() -> Config {
    let mut config = Config::default();
    config.providers.insert(
        "p".into(),
        ProviderProfileConfig {
            provider_type: ProviderTypeConfig::OpenAi,
            default_model: "m".into(),
            ..ProviderProfileConfig::default()
        },
    );
    for (name, model) in [("fast", "m"), ("scout", "m3"), ("other", "m2")] {
        config.routing.routes.insert(name.into(), route(model));
    }
    for (role, route) in [
        (&mut config.agents.worker.base, "fast"),
        (&mut config.agents.explorer, "scout"),
    ] {
        *role = RoleBindingConfig {
            models: vec![ChainEntryConfig::route(route)],
            ..RoleBindingConfig::default()
        };
    }
    config
}

fn with_description(mut config: Config, model: &str, tool: &str, text: &str) -> Config {
    config
        .model_tool_descriptions
        .entry("p".into())
        .or_default()
        .entry(model.into())
        .or_default()
        .insert(tool.into(), text.into());
    config
}

fn catalog() -> BuiltinCatalog {
    BuiltinCatalog::for_evaluation().unwrap()
}

#[test]
fn a_description_only_candidate_is_recorded_as_a_description_change_not_a_binding_swap() {
    // Given: a candidate that only rewrites `read` for the model the worker uses.
    let baseline = baseline();
    let candidate = with_description(baseline.clone(), "m", "read", "Read a file, briefly.");

    // When: the changes are derived.
    let changes = derive(&baseline, &candidate, &catalog()).unwrap();

    // Then: no binding differs, the description is recorded with both sides' hashes, and
    // only the roles that use that model and can see the tool are affected.
    assert!(changes.bindings.is_empty());
    assert_eq!(changes.affected_roles, ["worker"]);
    let [change] = changes.tool_descriptions.as_slice() else {
        panic!("one description change: {changes:?}");
    };
    assert_eq!(
        (change.profile.as_str(), change.route_model_id.as_str()),
        ("p", "m")
    );
    assert_eq!(change.tool, "read");
    assert_eq!(
        change.candidate_hash,
        sha256_hex("Read a file, briefly.".as_bytes())
    );
    let built_in = catalog().descriptions()["read"].clone();
    assert_eq!(change.baseline_hash, sha256_hex(built_in.as_bytes()));
    assert!(roles::swapped_roles(&baseline, &candidate).is_empty());
}

#[test]
fn a_binding_only_candidate_is_accepted_and_names_the_roles_it_swaps() {
    // Given: a candidate that moves the worker to another route.
    let baseline = baseline();
    let mut candidate = baseline.clone();
    candidate.agents.worker.base.models = vec![ChainEntryConfig::route("other")];

    // When: it is accepted.
    let changes = derive(&baseline, &candidate, &catalog()).unwrap();

    // Then: the binding difference is recorded and no description is.
    assert_eq!(changes.bindings, ["worker"]);
    assert_eq!(changes.affected_roles, ["worker"]);
    assert!(changes.tool_descriptions.is_empty());
}

#[test]
fn a_candidate_that_changes_nothing_is_refused() {
    // Given: a candidate equal to the baseline, and one whose override is the build's own text.
    let baseline = baseline();
    let built_in = catalog().descriptions()["read"].clone();
    let same_text = with_description(baseline.clone(), "m", "read", &built_in);

    // When / Then: neither changes anything effective.
    for candidate in [baseline.clone(), same_text] {
        let refused = derive(&baseline, &candidate, &catalog()).unwrap_err();
        assert!(refused.contains("changes nothing"), "{refused}");
    }
}

#[test]
fn only_fixed_builtin_tools_can_have_their_description_changed() {
    // Given: descriptions for the run_js tool, a team tool and an unknown name.
    let baseline = baseline();
    for tool in [
        "run_js",
        "task_claim",
        "mcp__server__search",
        "no_such_tool",
    ] {
        let candidate = with_description(baseline.clone(), "m", tool, "x");

        // When / Then: each is refused by name.
        let refused = derive(&baseline, &candidate, &catalog()).unwrap_err();
        assert!(
            refused.contains(&format!("`{tool}`")) && refused.contains("not a fixed builtin"),
            "{refused}"
        );
    }
}

#[test]
fn a_description_for_a_model_no_role_uses_or_a_tool_it_cannot_see_reaches_nobody() {
    // Given: an override for a model no chain names, and one for a tool the
    // read-only explorer, the only user of its model, is never offered.
    let baseline = baseline();
    let unused = with_description(baseline.clone(), "unused-model", "read", "x");
    let invisible = with_description(baseline.clone(), "m3", "write", "x");

    // When / Then: both are refused rather than run as a comparison of nothing.
    for candidate in [unused, invisible] {
        let refused = derive(&baseline, &candidate, &catalog()).unwrap_err();
        assert!(refused.contains("reaches no role"), "{refused}");
    }
}

/// The baseline, with the edit interface the provider profile `p` gives its models.
fn with_edit_tool(mut config: Config, edit_tool: config::EditToolConfig) -> Config {
    config.providers.get_mut("p").unwrap().edit_tool = Some(edit_tool);
    config
}

#[test]
fn a_description_the_models_edit_interface_never_sends_is_refused_before_any_request() {
    use config::EditToolConfig::{ApplyPatch, Edit};

    // Given: a model that uses apply-patch, so edit and write are not sent to it, and a model
    // that uses the edit interface, so apply_patch is not.
    for (edit_tool, tool) in [
        (ApplyPatch, "write"),
        (ApplyPatch, "edit"),
        (Edit, "apply_patch"),
        (ApplyPatch, "apply_patch"),
    ] {
        let baseline = with_edit_tool(baseline(), edit_tool);
        let candidate = with_description(baseline.clone(), "m", tool, "x");

        // When / Then: the comparison is refused and says why, not as "no role".
        let refused = derive(&baseline, &candidate, &catalog()).unwrap_err();
        assert!(
            refused.contains(&format!("`{tool}`")) && refused.contains("never sent"),
            "{edit_tool:?} {tool}: {refused}"
        );
    }
}

#[test]
fn a_description_of_the_tool_the_models_edit_interface_sends_is_accepted() {
    use config::EditToolConfig::{ApplyPatch, Edit};

    // Given: the edit interface and its own tool, and apply-patch with a tool it keeps.
    for (edit_tool, tool) in [(Edit, "edit"), (Edit, "write"), (ApplyPatch, "read")] {
        let baseline = with_edit_tool(baseline(), edit_tool);
        let candidate = with_description(baseline.clone(), "m", tool, "x");

        // When / Then: the description is a change that reaches the worker.
        let changes = derive(&baseline, &candidate, &catalog()).unwrap();
        assert_eq!(changes.affected_roles, ["worker"], "{edit_tool:?} {tool}");
    }
}

#[test]
fn a_model_change_together_with_a_description_change_is_not_a_description_comparison() {
    // Given: a candidate that moves the worker to another model and rewrites `read` for the old one.
    let baseline = baseline();
    let mut candidate = with_description(baseline.clone(), "m", "read", "x");
    candidate.agents.worker.base.models = vec![ChainEntryConfig::route("other")];

    // When / Then: it is refused because a difference could not be told apart.
    let refused = derive(&baseline, &candidate, &catalog()).unwrap_err();
    assert!(
        refused.contains("together with tool descriptions"),
        "{refused}"
    );
}

#[test]
fn a_preset_only_swap_has_the_same_model_settings_on_both_sides() {
    // Given: two configurations that differ only in the worker's preset name.
    let baseline = baseline();
    let mut candidate = baseline.clone();
    candidate.agents.worker.base.preset = Some("terse".into());

    // When / Then: their model settings are equal, while a route change is not.
    assert_eq!(
        roles::model_settings(&baseline, "worker"),
        roles::model_settings(&candidate, "worker")
    );
    candidate.agents.worker.base.models = vec![ChainEntryConfig::with_effort("fast", "high")];
    assert_ne!(
        roles::model_settings(&baseline, "worker"),
        roles::model_settings(&candidate, "worker")
    );
}
