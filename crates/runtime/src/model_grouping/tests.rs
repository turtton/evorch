use super::*;
use config::{ChainEntryConfig, ProviderTypeConfig};

const KIMI_K3: &str = "moonshotai/kimi-k3";
const POOL: &str = "moonshotai/kimi-k3";

async fn catalog(api: serde_json::Value) -> ModelCatalog {
    let dir = tempfile::tempdir().unwrap();
    let data = serde_json::json!({"fetched_at": 4_102_444_800_u64, "api": api});
    std::fs::write(dir.path().join("models-dev.json"), data.to_string()).unwrap();
    ModelCatalog::load_or_refresh(dir.path()).await.unwrap()
}

fn canonical(id: &str, canonical: &str) -> (String, serde_json::Value) {
    (
        id.to_owned(),
        serde_json::json!({"id": id, "canonical_model_id": canonical}),
    )
}

fn models(entries: impl IntoIterator<Item = (String, serde_json::Value)>) -> serde_json::Value {
    serde_json::json!({"models": entries.into_iter().collect::<serde_json::Map<_, _>>()})
}

async fn kimi_catalog() -> ModelCatalog {
    catalog(serde_json::json!({
        "kimi-code-plan-global": models([
            canonical("k3", KIMI_K3),
            canonical("k3-256k", KIMI_K3),
            canonical("kimi-for-coding", "moonshotai/kimi-k2.8-preview"),
        ]),
        "neuralwatt": models([
            canonical("kimi-k3", KIMI_K3),
            canonical("kimi-k3-flex", KIMI_K3),
        ]),
        "vivgrid": models([canonical("kimi-k3", KIMI_K3)]),
        "nocanon": models([("mystery".to_owned(), serde_json::json!({"id": "mystery"}))]),
    }))
    .await
}

fn provider(provider_type: ProviderTypeConfig, models: &[&str]) -> ProviderProfileConfig {
    ProviderProfileConfig {
        provider_type,
        models: models
            .iter()
            .copied()
            .map(ModelEntryConfig::enabled)
            .collect(),
        default_model: models[0].to_owned(),
        ..ProviderProfileConfig::default()
    }
}

fn providers(
    entries: impl IntoIterator<Item = (&'static str, ProviderProfileConfig)>,
) -> BTreeMap<String, ProviderProfileConfig> {
    entries
        .into_iter()
        .map(|(name, profile)| (name.to_owned(), profile))
        .collect()
}

fn candidate(profile: &str, model: &str, auto: bool) -> RouteCandidateConfig {
    RouteCandidateConfig {
        profile: profile.into(),
        model: Some(model.into()),
        auto,
    }
}

fn routing(
    routes: impl IntoIterator<Item = (&'static str, Vec<RouteCandidateConfig>)>,
) -> RoutingConfig {
    RoutingConfig {
        routes: routes
            .into_iter()
            .map(|(name, candidates)| (name.to_owned(), candidates))
            .collect(),
        ..RoutingConfig::default()
    }
}

fn planner_uses(route: &str) -> AgentsConfig {
    let mut agents = AgentsConfig::default();
    agents.roles.planner.models = vec![ChainEntryConfig::route(route)];
    agents
}

#[test]
fn tier_comes_from_the_speed_marker_or_a_provider_suffix_segment() {
    for (id, tier) in [
        ("gpt-6+fast", Tier::Fast),
        ("kimi-k3-fast", Tier::Fast),
        ("kimi-k3:fast", Tier::Fast),
        ("moonshotai/Kimi-K3-Fast", Tier::Fast),
        ("kimi-for-coding-highspeed", Tier::Fast),
        ("kimi-k3-flex", Tier::Flex),
        ("kimi-k3-eco", Tier::Eco),
        ("moonshotai/kimi-k3-free", Tier::Free),
        ("kimi-k3", Tier::Standard),
        ("k3-256k", Tier::Standard),
        ("breakfast-model", Tier::Standard),
    ] {
        assert_eq!(tier_of(id), tier, "{id}");
    }
}

#[tokio::test]
async fn a_new_provider_joins_the_pool_of_its_canonical_model_at_the_lowest_priority() {
    // Given: a pool served by the Kimi subscription, used by the planner.
    let catalog = kimi_catalog().await;
    let providers = providers([
        (
            "kimi",
            provider(ProviderTypeConfig::KimiSubscription, &["k3"]),
        ),
        (
            "neuralwatt",
            provider(ProviderTypeConfig::OpenAiCompatible, &["kimi-k3"]),
        ),
    ]);
    let routing = routing([(POOL, vec![candidate("kimi", "k3", false)])]);
    // When: neuralwatt is saved; its slug is inferred from the profile name.
    let plan = plan_for_profile(
        "neuralwatt",
        &providers,
        &routing,
        &planner_uses(POOL),
        &catalog,
    );
    // Then: the differently named model joins the pool as an auto candidate.
    assert_eq!(
        plan,
        AutoGroupPlan {
            additions: vec![AutoAddition {
                route: POOL.into(),
                candidate: candidate("neuralwatt", "kimi-k3", true),
            }],
            conflicts: vec![],
        }
    );
    let mut routing = routing;
    let applied = apply_additions(&mut routing, &providers, &plan.additions);
    assert_eq!(applied, plan.additions);
    assert_eq!(
        routing.routes[POOL],
        [
            candidate("kimi", "k3", false),
            candidate("neuralwatt", "kimi-k3", true)
        ]
    );
    // When: the same addition is applied again.
    assert!(apply_additions(&mut routing, &providers, &plan.additions).is_empty());
    // Then: no duplicate is created.
    assert_eq!(routing.routes[POOL].len(), 2);
}

#[tokio::test]
async fn several_matching_models_in_one_profile_are_a_conflict() {
    // Given: the subscription enables both k3 and k3-256k, which share the canonical id.
    let catalog = kimi_catalog().await;
    let providers = providers([
        (
            "neuralwatt",
            provider(ProviderTypeConfig::OpenAiCompatible, &["kimi-k3"]),
        ),
        (
            "kimi",
            provider(ProviderTypeConfig::KimiSubscription, &["k3", "k3-256k"]),
        ),
    ]);
    let routing = routing([(POOL, vec![candidate("neuralwatt", "kimi-k3", false)])]);
    // When
    let plan = plan_for_profile("kimi", &providers, &routing, &planner_uses(POOL), &catalog);
    // Then: nothing is added; the user chooses.
    assert!(plan.additions.is_empty());
    assert_eq!(
        plan.conflicts,
        [AutoConflict {
            route: POOL.into(),
            profile: "kimi".into(),
            models: vec!["k3".into(), "k3-256k".into()],
        }]
    );
}

#[tokio::test]
async fn a_different_tier_never_joins_the_pool() {
    // Given: the new profile only offers the flex tier of the same canonical model.
    let catalog = kimi_catalog().await;
    let providers = providers([
        (
            "vivgrid",
            provider(ProviderTypeConfig::OpenAiCompatible, &["kimi-k3"]),
        ),
        (
            "neuralwatt",
            provider(ProviderTypeConfig::OpenAiCompatible, &["kimi-k3-flex"]),
        ),
    ]);
    let routing = routing([(POOL, vec![candidate("vivgrid", "kimi-k3", false)])]);
    // When / Then
    let plan = plan_for_profile(
        "neuralwatt",
        &providers,
        &routing,
        &planner_uses(POOL),
        &catalog,
    );
    assert_eq!(plan, AutoGroupPlan::default());
}

#[tokio::test]
async fn unknown_identities_unused_pools_and_served_pools_are_left_alone() {
    let catalog = kimi_catalog().await;
    let providers = providers([
        (
            "vivgrid",
            provider(ProviderTypeConfig::OpenAiCompatible, &["kimi-k3"]),
        ),
        (
            "neuralwatt",
            provider(ProviderTypeConfig::OpenAiCompatible, &["kimi-k3"]),
        ),
        (
            "nocanon",
            provider(ProviderTypeConfig::OpenAiCompatible, &["mystery"]),
        ),
        (
            "other",
            provider(ProviderTypeConfig::OpenAiCompatible, &["mystery"]),
        ),
    ]);
    // Given: an unused pool, a pool the profile already serves, and a pool without identity.
    let routing = routing([
        ("unused", vec![candidate("vivgrid", "kimi-k3", false)]),
        (
            "served",
            vec![
                candidate("vivgrid", "kimi-k3", false),
                candidate("neuralwatt", "kimi-k3", false),
            ],
        ),
        ("mystery", vec![candidate("nocanon", "mystery", false)]),
    ]);
    let mut agents = planner_uses("served");
    agents.roles.advisor.models = vec![ChainEntryConfig::route("mystery")];
    // When / Then: none of them changes.
    for profile in ["neuralwatt", "other"] {
        assert_eq!(
            plan_for_profile(profile, &providers, &routing, &agents, &catalog),
            AutoGroupPlan::default(),
            "{profile}"
        );
    }
}

#[tokio::test]
async fn a_pool_used_by_the_role_name_fallback_counts_as_referenced() {
    // Given: the planner has no chain, so it uses the route named `planner`.
    let catalog = kimi_catalog().await;
    let providers = providers([
        (
            "vivgrid",
            provider(ProviderTypeConfig::OpenAiCompatible, &["kimi-k3"]),
        ),
        (
            "neuralwatt",
            provider(ProviderTypeConfig::OpenAiCompatible, &["kimi-k3"]),
        ),
    ]);
    let routing = routing([("planner", vec![candidate("vivgrid", "kimi-k3", false)])]);
    // When
    let plan = plan_for_profile(
        "neuralwatt",
        &providers,
        &routing,
        &AgentsConfig::default(),
        &catalog,
    );
    // Then
    assert_eq!(plan.additions.len(), 1);
    assert_eq!(plan.additions[0].route, "planner");
}

#[tokio::test]
async fn manually_described_models_are_never_grouped() {
    let catalog = kimi_catalog().await;
    let mut neuralwatt = provider(ProviderTypeConfig::OpenAiCompatible, &["kimi-k3"]);
    neuralwatt.models[0].metadata_source = Some(MetadataSource::Manual);
    assert_eq!(
        group_key("neuralwatt", &neuralwatt, "kimi-k3", &catalog),
        None
    );
}

#[tokio::test]
async fn a_changed_canonical_id_is_reported_without_rewriting() {
    // Given: an auto candidate whose catalog entry now names another model, and one the
    // catalog no longer lists.
    let catalog = catalog(serde_json::json!({
        "vivgrid": models([canonical("kimi-k3", KIMI_K3)]),
        "neuralwatt": models([canonical("kimi-k3", "moonshotai/kimi-k3.1")]),
    }))
    .await;
    let providers = providers([
        (
            "vivgrid",
            provider(ProviderTypeConfig::OpenAiCompatible, &["kimi-k3"]),
        ),
        (
            "neuralwatt",
            provider(ProviderTypeConfig::OpenAiCompatible, &["kimi-k3"]),
        ),
        (
            "gone",
            provider(ProviderTypeConfig::OpenAiCompatible, &["kimi-k3-old"]),
        ),
    ]);
    let routing = routing([(
        POOL,
        vec![
            candidate("vivgrid", "kimi-k3", false),
            candidate("neuralwatt", "kimi-k3", true),
            candidate("gone", "kimi-k3-old", true),
        ],
    )]);
    // When
    let stale = stale_auto_candidates(&routing, &providers, &catalog);
    // Then: only the confirmed change is reported.
    assert_eq!(
        stale,
        [StaleAutoCandidate {
            route: POOL.into(),
            profile: "neuralwatt".into(),
            model: "kimi-k3".into(),
            expected: GroupKey {
                canonical: KIMI_K3.into(),
                tier: Tier::Standard
            },
            actual: GroupKey {
                canonical: "moonshotai/kimi-k3.1".into(),
                tier: Tier::Standard
            },
        }]
    );
}
