use mock_openai::StreamingMockOpenAi;
use providers::{ProviderAuth, ProviderError};

#[tokio::test]
async fn connectivity_uses_authenticated_model_discovery() {
    // Given: a local model catalog.
    let server = StreamingMockOpenAi::spawn(vec![]);
    // When: connectivity is verified without requesting a completion.
    providers::verify_connectivity(&server.base_url(), &ProviderAuth::new("key"))
        .await
        .unwrap();
    // Then: exactly one authenticated catalog request is made.
    let requests = server.recorded_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/v1/models");
    assert_eq!(requests[0].authorization.as_deref(), Some("Bearer key"));
}

#[tokio::test]
async fn connectivity_preserves_http_failure() {
    // Given: a fixture returning 500 outside its catalog path.
    let server = StreamingMockOpenAi::spawn(vec![]);
    // When: the catalog cannot be reached successfully.
    let result = providers::verify_connectivity(
        &format!("{}/missing", server.base_url()),
        &ProviderAuth::new("key"),
    )
    .await;
    // Then: the existing provider HTTP error is preserved.
    assert!(matches!(
        result,
        Err(ProviderError::Http { status: 500, .. })
    ));
}
