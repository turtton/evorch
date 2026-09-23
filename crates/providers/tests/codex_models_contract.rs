use providers::{CODEX_MODELS_FALLBACK_VERSION, ProviderAuth, ProviderError};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn lists_new_slugs_using_resolved_catalog_version() {
    // Given: the backend advertises Sol/Luna only to newer clients.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/codex/models"))
        .and(header("authorization", "Bearer oauth-access"))
        .and(query_param("client_version", "0.156.1"))
        .and(header("chatgpt-account-id", "catalog-account"))
        .and(header("originator", "codex_cli_rs"))
        .and(header("user-agent", "codex_cli_rs/0.156.1"))
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
        "0.156.1",
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
    assert_eq!(providers::CODEX_INFERENCE_CLIENT_VERSION, "0.153.0");
}

#[tokio::test]
async fn catalog_advertises_fast_support_leniently() {
    // Given: 新旧tier形式・非対応・未知フィールドを含むカタログ。
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": [
            {"slug": "priority", "service_tiers": [{"id": "priority", "cost": 2.5}], "extra": true},
            {"slug": "legacy", "additional_speed_tiers": ["fast"]},
            {"slug": "standard"},
            {"slug": "other", "service_tiers": [{"id": "default"}], "additional_speed_tiers": ["slow"]},
            {"slug": "null", "service_tiers": null, "additional_speed_tiers": null}
        ]})))
        .expect(1)
        .mount(&server).await;
    // When: カタログを取得する。
    let models = providers::list_codex_models(
        &server.uri(),
        &ProviderAuth::new("token"),
        "account",
        CODEX_MODELS_FALLBACK_VERSION,
    )
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
            CODEX_MODELS_FALLBACK_VERSION,
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
        CODEX_MODELS_FALLBACK_VERSION,
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
        CODEX_MODELS_FALLBACK_VERSION,
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
        .and(query_param("client_version", CODEX_MODELS_FALLBACK_VERSION))
        .and(header("chatgpt-account-id", "catalog-account"))
        .and(header("originator", "codex_cli_rs"))
        .and(header("user-agent", "codex_cli_rs/0.156.1"))
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
        CODEX_MODELS_FALLBACK_VERSION,
    )
    .await;
    // Then
    assert!(
        matches!(result, Err(ProviderError::Http { status: 403, body }) if body == "client headers rejected")
    );
}
