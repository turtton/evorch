use config::{Config, RouteCandidateConfig};
use gui::model::routing_settings::{RoutingSettingsModel, route_effort_choices};

fn fixture() -> Config {
    let mut config = Config::default();
    config.providers.insert("local".into(), Default::default());
    config.routing.routes.insert(
        "worker".into(),
        vec![
            RouteCandidateConfig {
                profile: "local".into(),
                model: Some("custom".into()),
                auto: false,
            },
            RouteCandidateConfig {
                profile: "local".into(),
                model: None,
                auto: false,
            },
        ],
    );
    config.routing.routes.insert(
        "other".into(),
        vec![RouteCandidateConfig {
            profile: "local".into(),
            model: Some("external".into()),
            auto: false,
        }],
    );
    config
}

#[test]
fn seed_profile_models_excludes_disabled_entries() {
    // Given: a profile declaring an enabled model followed by a disabled model.
    let mut config = fixture();
    config.providers.get_mut("local").expect("profile").models = vec![
        config::ModelEntryConfig::enabled("enabled-model"),
        config::ModelEntryConfig {
            enabled: false,
            ..config::ModelEntryConfig::enabled("disabled-model")
        },
    ];
    // When: seeding the routing editor choices.
    let model = RoutingSettingsModel::seed_from_config(&config);
    // Then: only enabled IDs remain in declaration order.
    assert_eq!(model.profile_models["local"], ["enabled-model"]);
}

#[test]
fn route_effort_choices_follow_override_or_profile_default_model() {
    // Given: the profile default model has restricted levels, the override model has none.
    let mut config = fixture();
    let profile = config.providers.get_mut("local").expect("profile");
    profile.provider_type = config::ProviderTypeConfig::OpenAiCompatible;
    profile.default_model = "base".into();
    let mut base = config::ModelEntryConfig::enabled("base");
    base.effort_levels = Some(vec!["minimal".into(), "high".into()]);
    profile.models = vec![base, config::ModelEntryConfig::enabled("custom")];
    let [overridden, inherited] = config.routing.routes["worker"].clone().try_into().unwrap();
    config
        .routing
        .routes
        .insert("overridden".into(), vec![overridden]);
    config
        .routing
        .routes
        .insert("inherited".into(), vec![inherited]);
    // When/Then: the inherited default uses its levels, the override falls back to defaults.
    assert_eq!(
        route_effort_choices(&config, "inherited"),
        ["minimal", "high"]
    );
    assert_eq!(
        route_effort_choices(&config, "overridden"),
        gui::model::effort::DEFAULT_EFFORT_LEVELS.map(str::to_owned)
    );
    assert!(route_effort_choices(&config, "missing").is_empty());
}

#[test]
fn claude_routes_use_model_capabilities_and_restrict_configured_efforts() {
    let config: Config = toml::from_str(
        r#"
[providers.claude]
type = "anthropic-subscription"
default_model = "claude-sonnet-4-5"
models = [
  "claude-sonnet-4-5",
  { id = "claude-sonnet-5-5", effort_levels = ["max", "high", "none", "minimal", "xhigh"] },
  "claude-mythos-preview",
  { id = "claude-opus-4-7", effort_levels = [] },
  { id = "unknown", effort_levels = ["high"] },
]
[providers.local]
type = "openai-compatible"
default_model = "base"
models = [{ id = "base", effort_levels = ["minimal", "high"] }]
[routing.routes]
inherited = [{ profile = "claude" }]
overridden = [{ profile = "claude", model = "claude-sonnet-5-5" }]
required = [{ profile = "claude", model = "claude-mythos-preview" }]
empty = [{ profile = "claude", model = "claude-opus-4-7" }]
unknown = [{ profile = "claude", model = "unknown" }]
mixed = [{ profile = "claude" }, { profile = "claude", model = "claude-sonnet-5-5" }, { profile = "local" }]
"#,
    )
    .unwrap();
    assert_eq!(
        route_effort_choices(&config, "inherited"),
        ["none", "low", "medium", "high"]
    );
    assert_eq!(
        route_effort_choices(&config, "overridden"),
        ["max", "high", "xhigh"]
    );
    assert_eq!(
        route_effort_choices(&config, "required"),
        ["low", "medium", "high", "max"]
    );
    for route in ["empty", "unknown"] {
        assert!(route_effort_choices(&config, route).is_empty(), "{route}");
    }
    assert_eq!(
        route_effort_choices(&config, "mixed"),
        ["none", "low", "medium", "high", "max", "xhigh", "minimal"]
    );
}

#[test]
fn duplicate_add_and_rename_never_overwrite_candidates() {
    // Given: 既存の異なるルート。
    let mut model = RoutingSettingsModel::seed_from_config(&fixture());
    let before = model.routes.clone();
    // When: 衝突する名前を指定する。
    assert!(model.add_route("worker").is_err());
    assert!(model.rename_route("other", "worker").is_err());
    assert!(model.add_route("  ").is_err());
    // Then: 既存データを上書きしない。
    assert_eq!(model.routes, before);
}

#[test]
fn seed_tracks_implicit_users_only_for_role_name_fallbacks() {
    // Given: worker falls back to its role name while other has no implicit users.
    let mut config = fixture();
    // When: seeding the routing editor.
    let model = RoutingSettingsModel::seed_from_config(&config);
    // Then: every existing route has an entry, independent of explicit route users.
    assert_eq!(model.implicit_route_users["worker"], ["worker"]);
    assert!(model.implicit_route_users["other"].is_empty());
    assert!(model.route_users["worker"].is_empty());
    config.agents.worker.base.models = vec![config::ChainEntryConfig::route("worker")];
    let explicit = RoutingSettingsModel::seed_from_config(&config);
    assert!(explicit.implicit_route_users["worker"].is_empty());
    assert_eq!(explicit.route_users["worker"], ["worker"]);
}
