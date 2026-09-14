use providers::{CODEX_MODELS_CLIENT_VERSION, ProviderAuth, ProviderError};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn lists_slugs_when_codex_catalog_is_returned() {
    // Given
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/codex/models"))
        .and(header("authorization", "Bearer oauth-access"))
        .and(query_param("client_version", CODEX_MODELS_CLIENT_VERSION))
        .and(header("chatgpt-account-id", "catalog-account"))
        .and(header("originator", "codex_cli_rs"))
        .and(header("user-agent", "codex_cli_rs/0.153.0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"slug": "gpt-b", "display_name": "B"}, {"slug": "gpt-a"}]
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
    assert_eq!(models, ["gpt-b", "gpt-a"]);
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
        .and(query_param("client_version", CODEX_MODELS_CLIENT_VERSION))
        .and(header("chatgpt-account-id", "catalog-account"))
        .and(header("originator", "codex_cli_rs"))
        .and(header("user-agent", "codex_cli_rs/0.153.0"))
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
        matches!(result, Err(ProviderError::Http { status: 403, body })
        if body == "client headers rejected")
    );
}
