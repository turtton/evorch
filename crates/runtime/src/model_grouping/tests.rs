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

#[tokio::test]
async fn image_pools_never_plan_unsupported_providers_even_as_conflicts() {
    let catalog = catalog(serde_json::json!({
        "openai": models([canonical("gpt-image-1", "image")]),
        "local": models([
            canonical("gpt-image-1", "image"),
            canonical("gpt-image-2", "image"),
        ]),
    }))
    .await;
    let mut agents = AgentsConfig::default();
    agents.roles.image_generator.models = vec![ChainEntryConfig::route("art")];
    for ids in [&["gpt-image-1"][..], &["gpt-image-1", "gpt-image-2"][..]] {
        for provider_type in [
            ProviderTypeConfig::OpenAiCompatible,
            ProviderTypeConfig::OpenAi,
            ProviderTypeConfig::OpenAiCodex,
        ] {
            let providers = providers([
                (
                    "openai",
                    provider(ProviderTypeConfig::OpenAi, &["gpt-image-1"]),
                ),
                ("local", provider(provider_type, ids)),
            ]);
            let routing = routing([("art", vec![candidate("openai", "gpt-image-1", false)])]);
            let plan = plan_for_profile("local", &providers, &routing, &agents, &catalog);
            if provider_type == ProviderTypeConfig::OpenAiCompatible {
                assert_eq!(
                    plan,
                    AutoGroupPlan::default(),
                    "unsupported candidates must not be added or offered"
                );
            } else if ids.len() == 1 {
                assert_eq!(plan.additions.len(), 1);
                assert!(plan.conflicts.is_empty());
            } else {
                assert!(plan.additions.is_empty());
                assert_eq!(plan.conflicts.len(), 1);
            }
        }
    }
    // A route used by both conversation and image chains cannot accept either kind.
    agents.worker.base.models = vec![ChainEntryConfig::route("art")];
    let providers = providers([
        (
            "openai",
            provider(ProviderTypeConfig::OpenAi, &["gpt-image-1"]),
        ),
        (
            "local",
            provider(ProviderTypeConfig::OpenAi, &["gpt-image-1"]),
        ),
    ]);
    let routing = routing([("art", vec![candidate("openai", "gpt-image-1", false)])]);
    assert_eq!(
        plan_for_profile("local", &providers, &routing, &agents, &catalog),
        AutoGroupPlan::default()
    );
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
                tier: Tier::Standard,
                kind: ModelKind::Chat,
            },
            actual: GroupKey {
                canonical: "moonshotai/kimi-k3.1".into(),
                tier: Tier::Standard,
                kind: ModelKind::Chat,
            },
        }]
    );
}

const GPT_IMAGE: &str = "openai/gpt-image-2";

async fn image_catalog() -> ModelCatalog {
    catalog(serde_json::json!({
        "openai": models([
            canonical("gpt-image-2", GPT_IMAGE),
            canonical("gpt-image-2-chat", GPT_IMAGE),
        ]),
    }))
    .await
}

fn with_kind(
    mut profile: ProviderProfileConfig,
    id: &str,
    kind: ModelKind,
) -> ProviderProfileConfig {
    let entry = profile
        .models
        .iter_mut()
        .find(|entry| entry.id == id)
        .expect("registered model");
    entry.kind = Some(kind);
    profile
}

#[tokio::test]
async fn the_group_key_carries_the_model_kind() {
    // Given: the same catalog identity registered as an image model and as a chat model.
    let catalog = image_catalog().await;
    let image = provider(
        ProviderTypeConfig::OpenAiCodex,
        &["gpt-6-astra", "gpt-image-2"],
    );
    let chat = with_kind(
        provider(ProviderTypeConfig::OpenAi, &["gpt-image-2"]),
        "gpt-image-2",
        ModelKind::Chat,
    );
    // When / Then: only the kind tells the two apart.
    let image_key = group_key("codex", &image, "gpt-image-2", &catalog).expect("image key");
    let chat_key = group_key("openai", &chat, "gpt-image-2", &catalog).expect("chat key");
    assert_eq!(image_key.kind, ModelKind::ImageGeneration);
    assert_eq!(chat_key.kind, ModelKind::Chat);
    assert_eq!(image_key.canonical, chat_key.canonical);
    assert_ne!(image_key, chat_key);
}

#[tokio::test]
async fn models_of_another_kind_never_join_a_pool() {
    // Given: an image pool used by the image generator, and a chat pool used by the
    // planner, both on the same catalog identity.
    let catalog = image_catalog().await;
    let mut agents = planner_uses("chat-pool");
    agents.roles.image_generator.models = vec![ChainEntryConfig::route("image-pool")];
    let routing = routing([
        ("image-pool", vec![candidate("codex", "gpt-image-2", false)]),
        (
            "chat-pool",
            vec![candidate("compatible", "gpt-image-2-chat", false)],
        ),
    ]);
    let compatible = with_kind(
        provider(ProviderTypeConfig::OpenAiCompatible, &["gpt-image-2-chat"]),
        "gpt-image-2-chat",
        ModelKind::Chat,
    );

    // When: an OpenAI profile whose gpt-image-2 is an image model is saved.
    let image_providers = providers([
        (
            "codex",
            provider(
                ProviderTypeConfig::OpenAiCodex,
                &["gpt-6-astra", "gpt-image-2"],
            ),
        ),
        ("compatible", compatible.clone()),
        (
            "openai",
            provider(ProviderTypeConfig::OpenAi, &["gpt-5", "gpt-image-2"]),
        ),
    ]);
    let plan = plan_for_profile("openai", &image_providers, &routing, &agents, &catalog);
    // Then: it joins the image pool only; the chat pool of the same identity is left alone.
    assert_eq!(
        plan,
        AutoGroupPlan {
            additions: vec![AutoAddition {
                route: "image-pool".into(),
                candidate: candidate("openai", "gpt-image-2", true),
            }],
            conflicts: vec![],
        }
    );

    // When: the same model is marked as a chat model instead.
    let chat_providers = providers([
        (
            "codex",
            provider(
                ProviderTypeConfig::OpenAiCodex,
                &["gpt-6-astra", "gpt-image-2"],
            ),
        ),
        ("compatible", compatible),
        (
            "openai",
            with_kind(
                provider(ProviderTypeConfig::OpenAi, &["gpt-5", "gpt-image-2"]),
                "gpt-image-2",
                ModelKind::Chat,
            ),
        ),
    ]);
    let plan = plan_for_profile("openai", &chat_providers, &routing, &agents, &catalog);
    // Then: it joins the chat pool only and is never offered to the image pool.
    assert_eq!(
        plan.additions,
        [AutoAddition {
            route: "chat-pool".into(),
            candidate: candidate("openai", "gpt-image-2", true),
        }]
    );
    assert!(plan.conflicts.is_empty());
}
