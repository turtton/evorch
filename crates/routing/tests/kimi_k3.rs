use std::collections::BTreeMap;

use model::{
    ApiProtocol, Availability, Capability, CapabilitySupport, LogicalModelId, ModelCatalog,
    ProviderType,
};
use routing::{CredentialRef, ProviderProfile, ResolvedRoute, Router, SessionAffinity};

fn tool_router(catalog: ModelCatalog, models: [&str; 2]) -> Router {
    let profiles = [
        ("kimi", ProviderType::KimiSubscription, models[0]),
        ("neuralwatt", ProviderType::OpenAiCompatible, models[1]),
    ]
    .map(|(name, provider_type, model)| ProviderProfile {
        name: name.into(),
        provider_type,
        api_protocol: ApiProtocol::OpenAiCompletions,
        base_url: "http://localhost:1/v1".into(),
        credential: CredentialRef::Env {
            var: "TEST_KEY".into(),
        },
        models: vec![model.into()],
        default_model: model.into(),
    });
    Router::new(
        profiles.into(),
        &config::RoutingConfig {
            routes: BTreeMap::from([(
                "kimi-k3".into(),
                ["kimi", "neuralwatt"]
                    .into_iter()
                    .zip(models)
                    .map(|(profile, model)| config::RouteCandidateConfig {
                        profile: profile.into(),
                        model: Some(model.into()),
                    })
                    .collect(),
            )]),
        },
        catalog,
    )
    .expect("valid Kimi K3 route")
    .requiring_capability(Capability::ToolCalling)
}

#[test]
fn resolves_k3_when_discovered_capability_is_unknown() {
    // Given: config-declared models have only unconfirmed discovered placeholders.
    let catalog = discovered_catalog();
    let router = tool_router(catalog, ["k3", "kimi-k3"]);

    // When: resolving the orchestrator's logical model.
    let result = router.resolve(
        &mut SessionAffinity::default(),
        "run",
        &LogicalModelId::from("kimi-k3"),
    );

    // Then: the first candidate is selected without confirmed tool support.
    assert_eq!(
        result,
        Ok(ResolvedRoute {
            profile: "kimi".into(),
            model_id: "k3".into(),
        })
    );
}

#[test]
fn resolves_kimi_k3_when_first_candidate_is_unavailable() {
    // Given: the subscription model is unavailable, leaving an unknown fallback.
    let mut catalog = discovered_catalog();
    let mut unavailable = catalog.get("k3").expect("known candidate").clone();
    unavailable.availability = Availability::Unavailable;
    catalog.merge_models_dev(vec![unavailable]);
    let router = tool_router(catalog, ["k3", "kimi-k3"]);

    // When: resolving with tool calling required.
    let result = router.resolve(
        &mut SessionAffinity::default(),
        "run",
        &LogicalModelId::from("kimi-k3"),
    );

    // Then: the unknown second candidate is selected.
    assert_eq!(
        result,
        Ok(ResolvedRoute {
            profile: "neuralwatt".into(),
            model_id: "kimi-k3".into(),
        })
    );
}

#[test]
fn resolves_fast_variant_when_base_capability_is_unknown() {
    // Given: discovered fast variants have no confirmed capabilities of their own.
    for (first_availability, profile, model_id) in [
        (Availability::Available, "kimi", "k3+fast"),
        (Availability::Unavailable, "neuralwatt", "kimi-k3+fast"),
    ] {
        let mut catalog = discovered_catalog();
        catalog.merge_discovered(vec!["k3+fast".into(), "kimi-k3+fast".into()]);
        let mut first = catalog.get("k3+fast").expect("discovered variant").clone();
        first.availability = first_availability;
        catalog.merge_models_dev(vec![first]);
        let router = tool_router(catalog, ["k3+fast", "kimi-k3+fast"]);

        // When: resolving a speed variant with tool calling required.
        let result = router.resolve(
            &mut SessionAffinity::default(),
            "run",
            &LogicalModelId::from("kimi-k3"),
        );

        // Then: capability checks use the base ID while selection preserves the variant.
        assert_eq!(
            result,
            Ok(ResolvedRoute {
                profile: profile.into(),
                model_id: model_id.into(),
            })
        );
    }
}

fn discovered_catalog() -> ModelCatalog {
    let mut catalog = ModelCatalog::builtin();
    for id in ["k3", "kimi-k3"] {
        assert!(catalog.get(id).is_none(), "K3 must not be builtin");
    }
    catalog.merge_discovered(vec!["k3".into(), "kimi-k3".into()]);
    for id in ["k3", "kimi-k3"] {
        assert_eq!(
            catalog.capability_support(id, Capability::ToolCalling),
            CapabilitySupport::Unknown
        );
    }
    catalog
}

#[test]
fn skips_explicitly_unsupported_base_including_fast_variants() {
    // Given: only the first base model explicitly declares tools unsupported.
    for models in [["k3", "kimi-k3"], ["k3+fast", "kimi-k3+fast"]] {
        let mut catalog = discovered_catalog();
        catalog.merge_discovered(models.map(String::from).into());
        let mut unsupported = catalog.get("k3").expect("discovered base").clone();
        unsupported.capabilities.tool_calling = false;
        unsupported.attributes_confirmed = true;
        catalog.merge_models_dev(vec![unsupported]);
        let router = tool_router(catalog, models);

        // When: resolving either standard or fast candidates.
        let result = router.resolve(
            &mut SessionAffinity::default(),
            "run",
            &LogicalModelId::from("kimi-k3"),
        );

        // Then: the unsupported base is skipped even when its variant is unknown.
        assert_eq!(
            result,
            Ok(ResolvedRoute {
                profile: "neuralwatt".into(),
                model_id: models[1].into(),
            })
        );
    }
}
