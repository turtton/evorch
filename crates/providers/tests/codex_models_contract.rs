use providers::{CODEX_CLIENT_VERSION, ProviderAuth, ProviderError};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn lists_new_slugs_using_resolved_catalog_version() {
    // Given: the backend advertises Sol/Luna only to newer clients.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/codex/models"))
        .and(header("authorization", "Bearer oauth-access"))
        .and(query_param("client_version", CODEX_CLIENT_VERSION))
        .and(header("chatgpt-account-id", "catalog-account"))
        .and(header("originator", "codex_cli_rs"))
        .and(header(
            "user-agent",
            format!("codex_cli_rs/{CODEX_CLIENT_VERSION}"),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"slug": "gpt-6-sol", "display_name": "GPT-6-Sol"}, {"slug": "gpt-6-luna"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    // When
    let models = providers::list_codex_models(
        &format!("{}/backend-api/codex/", server.uri()),
        &ProviderAuth::new("oauth-access"),
        "catalog-account",
    )
    .await
    .unwrap();
    // Then
    assert_eq!(
        models
            .iter()
            .map(|info| info.slug.as_str())
            .collect::<Vec<_>>(),
        ["gpt-6-sol", "gpt-6-luna"]
    );
}

#[tokio::test]
async fn catalog_advertises_fast_support_leniently() {
    // Given: 新旧tier形式・非対応・未知フィールドを含むカタログ。
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": [
            {"slug": "priority", "context_window": 272000, "service_tiers": [{"id": "priority", "cost": 2.5}], "extra": true},
            {"slug": "legacy", "context_window": 0, "additional_speed_tiers": ["fast"]},
            {"slug": "standard"},
            {"slug": "other", "service_tiers": [{"id": "default"}], "additional_speed_tiers": ["slow"]},
            {"slug": "null", "context_window": null, "service_tiers": null, "additional_speed_tiers": null}
        ]})))
        .expect(1)
        .mount(&server).await;
    // When: カタログを取得する。
    let models =
        providers::list_codex_models(&server.uri(), &ProviderAuth::new("token"), "account")
            .await
            .expect("catalog");
    // Then: 広告されたpriorityまたはfastだけが対応扱いになる。
    let actual: Vec<_> = models
        .iter()
        .map(|info| (info.slug.as_str(), info.supports_fast))
        .collect();
    assert_eq!(
        actual,
        [
            ("priority", true),
            ("legacy", true),
            ("standard", false),
            ("other", false),
            ("null", false)
        ]
    );
    assert_eq!(models[0].context_window, Some(272_000));
    assert_eq!(models[1].context_window, None); // Zero is not a usable window.
    assert_eq!(models[2].context_window, None); // Older catalog may omit it.
    assert_eq!(models[4].context_window, None);
}

#[tokio::test]
async fn rejects_invalid_codex_response_when_schema_is_wrong() {
    // Given
    for body in [
        "not-json",
        "{}",
        r#"{"models":[{}]}"#,
        r#"{"models":"wrong"}"#,
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        // When
        let result = providers::list_codex_models(
            &server.uri(),
            &ProviderAuth::new("token"),
            "catalog-account",
        )
        .await;
        // Then
        assert!(matches!(result, Err(ProviderError::InvalidJson { .. })));
    }
}

#[tokio::test]
async fn returns_empty_when_codex_catalog_is_empty() {
    // Given
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": []})))
        .mount(&server)
        .await;
    // When
    let result = providers::list_codex_models(
        &server.uri(),
        &ProviderAuth::new("token"),
        "catalog-account",
    )
    .await;
    // Then
    assert!(result.unwrap().is_empty());
}

#[tokio::test]
async fn returns_http_error_when_oauth_is_rejected() {
    // Given
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401).set_body_string("denied"))
        .mount(&server)
        .await;
    // When
    let result = providers::list_codex_models(
        &server.uri(),
        &ProviderAuth::new("token"),
        "catalog-account",
    )
    .await;
    // Then
    assert!(matches!(
        result,
        Err(ProviderError::Http { status: 401, .. })
    ));
}

#[tokio::test]
async fn returns_http_error_when_strict_catalog_rejects_client_headers() {
    // Given
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("authorization", "Bearer oauth-access"))
        .and(query_param("client_version", CODEX_CLIENT_VERSION))
        .and(header("chatgpt-account-id", "catalog-account"))
        .and(header("originator", "codex_cli_rs"))
        .and(header(
            "user-agent",
            format!("codex_cli_rs/{CODEX_CLIENT_VERSION}"),
        ))
        .respond_with(ResponseTemplate::new(403).set_body_string("client headers rejected"))
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": []})))
        .with_priority(2)
        .expect(0)
        .mount(&server)
        .await;
    // When
    let result = providers::list_codex_models(
        &server.uri(),
        &ProviderAuth::new("oauth-access"),
        "catalog-account",
    )
    .await;
    // Then
    assert!(
        matches!(result, Err(ProviderError::Http { status: 403, body }) if body == "client headers rejected")
    );
}

const CATALOG_0166: &str = include_str!("fixtures/codex/models_catalog_0166.json");

async fn fetch(body: serde_json::Value) -> Vec<providers::CodexModelInfo> {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    providers::list_codex_models(&server.uri(), &ProviderAuth::new("token"), "account")
        .await
        .expect("catalog")
}

#[tokio::test]
async fn catalog_capabilities_follow_the_0166_models_json() {
    // Given: gpt-6-astra, gpt-6.1-sol and gpt-5.5 as shipped in Codex CLI 0.166.0
    let body = serde_json::from_str(CATALOG_0166).unwrap();
    // When
    let models = fetch(body).await;
    // Then: the Lite models are Lite with a low default effort; summary is judged by its
    // support flag alone (their default_reasoning_summary is "none"), verbosity follows the model
    let [astra, sol, gpt55] = &models[..] else {
        panic!("three models: {models:?}")
    };
    for lite in [astra, sol] {
        let caps = &lite.capabilities;
        assert!(caps.use_responses_lite, "{}", lite.slug);
        assert_eq!(caps.default_reasoning_level.as_deref(), Some("low"));
        assert_eq!(
            caps.supported_reasoning_levels,
            ["low", "medium", "high", "xhigh", "max", "ultra"]
        );
        assert!(caps.supports_reasoning_summary_parameter);
        assert!(caps.support_verbosity);
        assert_eq!(caps.default_verbosity.as_deref(), Some("low"));
        assert_eq!(caps.priority_tier, Some(true));
        assert_eq!(lite.context_window, Some(272_000));
        assert!(lite.supports_fast);
    }
    assert!(!gpt55.capabilities.use_responses_lite);
    assert_eq!(
        gpt55.capabilities.default_reasoning_level.as_deref(),
        Some("medium")
    );
}

/// What a fetched entry with no capability fields describes: the legacy defaults, and no
/// advertised `priority` tier.
fn fetched_default() -> providers::CodexModelCapabilities {
    providers::CodexModelCapabilities {
        priority_tier: Some(false),
        ..providers::CodexModelCapabilities::default()
    }
}

#[tokio::test]
async fn catalog_without_capability_fields_keeps_the_legacy_shape_except_for_tiers() {
    // Given: an older catalog entry that lists only the slug
    // When / Then: its capabilities equal the default except that the fetched entry does not
    // advertise priority (the wire stops sending it); the rest keeps the legacy request shape
    let models = fetch(serde_json::json!({"models": [{"slug": "plain"}]})).await;
    assert_eq!(models[0].capabilities, fetched_default());
    assert!(models[0].capabilities.supports_reasoning_summary_parameter);
}

#[tokio::test]
async fn catalog_tolerates_malformed_capability_fields() {
    // Given: capability fields of unexpected types, plus unknown fields
    let models = fetch(serde_json::json!({"models": [{
        "slug": "odd", "context_window": 100,
        "use_responses_lite": "yes",
        "default_reasoning_level": {"nested": true},
        "supported_reasoning_levels": "bad",
        "supports_reasoning_summary_parameter": "nope",
        "support_verbosity": null,
        "default_verbosity": 5,
        "future_field": [1, 2, 3]
    }, {
        "slug": "plain-levels", "supported_reasoning_levels": ["low", {"effort": "high"}, 3]
    }]}))
    .await;
    // Then: the catalog is accepted and the odd fields fall back to their defaults
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].context_window, Some(100));
    assert_eq!(models[0].capabilities, fetched_default());
    // A mixed list is malformed as a whole and falls back as a whole.
    assert!(models[1].capabilities.supported_reasoning_levels.is_empty());
}

#[tokio::test]
async fn catalog_context_window_falls_back_to_max_and_priority_follows_advertised_tiers() {
    // Given: windows with and without a usable context_window, and tier advertisements
    let models = fetch(serde_json::json!({"models": [
        {"slug": "max-only", "max_context_window": 500},
        {"slug": "zero", "context_window": 0, "max_context_window": 400},
        {"slug": "both", "context_window": 272, "max_context_window": 872},
        {"slug": "no-tiers"},
        {"slug": "empty-tiers", "service_tiers": []},
        {"slug": "other-tier", "service_tiers": [{"id": "flex"}]},
        {"slug": "priority", "service_tiers": [{"id": "priority"}]},
        {"slug": "legacy-fast", "additional_speed_tiers": ["fast"]},
        {"slug": "empty-and-fast", "service_tiers": [], "additional_speed_tiers": ["fast"]}
    ]}))
    .await;
    // Then
    let windows: Vec<_> = models.iter().map(|model| model.context_window).collect();
    assert_eq!(windows[..3], [Some(500), Some(400), Some(272)]);
    let tiers: Vec<_> = models
        .iter()
        .map(|model| model.capabilities.priority_tier)
        .collect();
    // Only `service_tiers` advertises a wire tier; `fast` alone is the UI's fast support.
    assert_eq!(
        tiers[3..],
        [
            Some(false),
            Some(false),
            Some(false),
            Some(true),
            Some(false),
            Some(false)
        ]
    );
    let fast: Vec<_> = models.iter().map(|model| model.supports_fast).collect();
    assert_eq!(fast[3..], [false, false, false, true, true, true]);
}

#[tokio::test]
async fn malformed_tier_lists_advertise_nothing_without_rejecting_the_catalog() {
    // Given: models with malformed tier lists next to a healthy Lite model
    let models = fetch(serde_json::json!({"models": [
        {"slug": "empty-object", "service_tiers": [{}]},
        {"slug": "wrong-ids", "service_tiers": [{"id": 5}, "priority", null],
            "additional_speed_tiers": [1, {"fast": true}]},
        {"slug": "not-lists", "service_tiers": "priority", "additional_speed_tiers": 7},
        {"slug": "mixed", "service_tiers": [{}, {"id": "priority"}],
            "additional_speed_tiers": [3, "fast"]},
        {"slug": "gpt-6-astra", "use_responses_lite": true, "default_reasoning_level": "low",
            "service_tiers": [{"id": "priority"}]}
    ]}))
    .await;
    // Then: the catalog is accepted, bad lists advertise nothing, good entries survive
    assert_eq!(models.len(), 5);
    let view: Vec<_> = models
        .iter()
        .map(|model| (model.capabilities.priority_tier, model.supports_fast))
        .collect();
    assert_eq!(
        view,
        [
            (Some(false), false),
            (Some(false), false),
            (Some(false), false),
            (Some(true), true),
            (Some(true), true)
        ]
    );
    let astra = &models[4].capabilities;
    assert!(astra.use_responses_lite);
    assert_eq!(astra.default_reasoning_level.as_deref(), Some("low"));
}
