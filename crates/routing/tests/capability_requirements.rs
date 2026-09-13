use std::collections::BTreeMap;

use model::{ApiProtocol, Capability, LogicalModelId, ModelCatalog, ProviderType};
use routing::{
    CredentialRef, FailureKind, ProviderProfile, ResolvedRoute, Router, RoutingError,
    SessionAffinity,
};

fn router(models: &[&str]) -> Router {
    let mut catalog = ModelCatalog::builtin();
    catalog.merge_discovered(vec!["unknown".into()]);
    let mut unsupported = catalog.get("gpt-4o").expect("builtin").clone();
    unsupported.model_id = "unsupported".into();
    unsupported.capabilities.tool_calling = false;
    catalog.merge_models_dev(vec![unsupported]);
    let profiles = models
        .iter()
        .map(|id| ProviderProfile {
            name: (*id).into(),
            provider_type: ProviderType::OpenAiCompatible,
            api_protocol: ApiProtocol::OpenAiCompletions,
            base_url: "http://localhost:1/v1".into(),
            credential: CredentialRef::Env {
                var: "TEST_KEY".into(),
            },
            models: vec![(*id).into()],
            default_model: (*id).into(),
        })
        .collect();
    Router::new(
        profiles,
        &config::RoutingConfig {
            routes: BTreeMap::from([(
                "worker".into(),
                models
                    .iter()
                    .map(|id| config::RouteCandidateConfig {
                        profile: (*id).into(),
                        model: None,
                    })
                    .collect(),
            )]),
        },
        catalog,
    )
    .expect("valid router")
}

#[test]
fn resolve_degrades_when_tool_support_is_unknown_or_unsupported() {
    // Given: only unknown or explicitly unsupported tool calling candidates.
    for id in ["unknown", "unsupported"] {
        let router = router(&[id]).requiring_capability(Capability::ToolCalling);
        // When: resolving a tool-calling flow.
        let result = router.resolve(
            &mut SessionAffinity::default(),
            "run",
            &LogicalModelId::from("worker"),
        );
        // Then: no optimistic selection.
        assert_eq!(
            result,
            Err(RoutingError::NoAvailableCandidate("worker".into()))
        );
    }
}

#[test]
fn supported_route_is_unchanged_when_tools_are_required() {
    // Given: confirmed builtin candidates in declared order.
    let router = router(&["gpt-4o", "gpt-4o-mini"]);
    let logical = LogicalModelId::from("worker");
    let expected = router.resolve(&mut SessionAffinity::default(), "run", &logical);
    // When: requiring tools.
    let actual = router
        .requiring_capability(Capability::ToolCalling)
        .resolve(&mut SessionAffinity::default(), "run", &logical);
    // Then: selection is identical.
    assert_eq!(actual, expected);
}

#[test]
fn pin_is_skipped_when_tool_support_is_unknown() {
    // Given: a text-only flow previously pinned an unknown model.
    let router = router(&["unknown", "gpt-4o"]).requiring_capability(Capability::ToolCalling);
    let mut affinity = SessionAffinity::default();
    affinity.pin("run", "worker", "unknown");
    // When: resolving a tool-calling flow with that pin.
    let route = router
        .resolve(&mut affinity, "run", &LogicalModelId::from("worker"))
        .expect("supported candidate");
    // Then: the pin cannot bypass the requirement.
    assert_eq!(route.model_id, "gpt-4o");
}

#[test]
fn fallback_skips_models_when_tool_support_is_not_supported() {
    // Given: unsupported candidates between two supported models.
    let router = router(&["gpt-4o", "unknown", "unsupported", "gpt-4o-mini"])
        .requiring_capability(Capability::ToolCalling);
    // When: the first model fails.
    let route = router.next_fallback(
        &mut SessionAffinity::default(),
        "run",
        &LogicalModelId::from("worker"),
        &ResolvedRoute {
            profile: "gpt-4o".into(),
            model_id: "gpt-4o".into(),
        },
        FailureKind::Server,
        None,
    );
    // Then: only a supported fallback is selected.
    assert_eq!(
        route.map(|route| route.model_id),
        Some("gpt-4o-mini".into())
    );
}

#[test]
fn fallback_degrades_when_only_unknown_models_remain() {
    // Given: a supported model followed by an unknown model.
    let router = router(&["gpt-4o", "unknown"]).requiring_capability(Capability::ToolCalling);
    // When: the supported model fails.
    let route = router.next_fallback(
        &mut SessionAffinity::default(),
        "run",
        &LogicalModelId::from("worker"),
        &ResolvedRoute {
            profile: "gpt-4o".into(),
            model_id: "gpt-4o".into(),
        },
        FailureKind::Server,
        None,
    );
    // Then: no optimistic fallback.
    assert_eq!(route, None);
}
