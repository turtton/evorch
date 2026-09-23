use std::collections::BTreeMap;

use model::{ApiProtocol, Capability, LogicalModelId, ModelCatalog, ProviderType};
use routing::{
    CredentialRef, FailureKind, ProviderProfile, ResolvedRoute, Router, RoutingError,
    SessionAffinity,
};

fn router(models: &[&str]) -> Router {
    let mut catalog = ModelCatalog::new();
    catalog.merge_discovered(vec![
        "gpt-4o".into(),
        "gpt-4o-mini".into(),
        "unknown".into(),
        "unsupported".into(),
    ]);
    let mut confirmed = Vec::new();
    for id in ["gpt-4o", "gpt-4o-mini", "unsupported"] {
        let mut entry = catalog.get(id).expect("fixture placeholder").clone();
        entry.capabilities.tool_calling = id != "unsupported";
        confirmed.push(entry);
    }
    catalog.merge_models_dev(confirmed);
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
fn resolve_degrades_when_tool_support_is_explicitly_unsupported() {
    // Given: an explicitly unsupported tool calling candidate.
    let router = router(&["unsupported"]).requiring_capability(Capability::ToolCalling);
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

#[test]
fn supported_route_is_unchanged_when_tools_are_required() {
    // Given: confirmed external-catalog candidates in declared order.
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
fn pin_is_preserved_when_tool_support_is_unknown() {
    // Given: a text-only flow previously pinned an unknown model.
    let router = router(&["unknown", "gpt-4o"]).requiring_capability(Capability::ToolCalling);
    let mut affinity = SessionAffinity::default();
    affinity.pin("run", "worker", "unknown", "unknown");
    // When: resolving a tool-calling flow with that pin.
    let route = router
        .resolve(&mut affinity, "run", &LogicalModelId::from("worker"))
        .expect("supported candidate");
    // Then: unknown support does not invalidate the pin.
    assert_eq!(route.model_id, "unknown");
}

#[test]
fn fallback_skips_explicitly_unsupported_models_but_accepts_unknown() {
    // Given: unsupported candidates between two supported models.
    let router = router(&["gpt-4o", "unsupported", "unknown", "gpt-4o-mini"])
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
    // Then: the unsupported candidate is skipped and the unknown one is selected.
    assert_eq!(route.map(|route| route.model_id), Some("unknown".into()));
}

#[test]
fn fallback_accepts_when_only_unknown_models_remain() {
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
    // Then: the unknown fallback remains eligible.
    assert_eq!(route.map(|route| route.model_id), Some("unknown".into()));
}
