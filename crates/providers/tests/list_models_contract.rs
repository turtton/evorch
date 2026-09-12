use std::time::Duration;

use mock_openai::{StreamingMockOpenAi, WriteMode};
use providers::{ProviderAuth, ProviderError};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn lists_ids_when_authenticated_get_succeeds() {
    // Given: an OpenAI fixture serving ordered model IDs and a trailing-slash URL.
    let server = StreamingMockOpenAi::spawn_with_models(
        vec![],
        WriteMode::default(),
        vec!["model-b".into(), "model-a".into()],
    );
    let base_url = format!("{}///", server.base_url());

    // When: discovering models through the public API.
    let result = providers::list_models(&base_url, &ProviderAuth::new("sk-test")).await;

    // Then: IDs preserve response order and the wire request is authenticated GET.
    let requests = server.recorded_requests();
    assert_eq!(result.unwrap(), ["model-b", "model-a"]);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/v1/models");
    assert_eq!(requests[0].authorization.as_deref(), Some("Bearer sk-test"));
}

#[tokio::test]
async fn returns_empty_when_list_data_is_omitted() {
    // Given: a compatible endpoint omitting defaultable metadata and data.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;
    // When: discovering models.
    let result = providers::list_models(&server.uri(), &ProviderAuth::new("key")).await;
    // Then: the omitted collection defaults to empty.
    assert_eq!(result.unwrap(), Vec::<String>::new());
}

#[tokio::test]
async fn returns_provider_errors_when_http_or_json_is_invalid() {
    // Given: status failures and malformed JSON/schema responses.
    for (status, body, expected) in [
        (
            401,
            "denied",
            ProviderError::Http {
                status: 401,
                body: "denied".into(),
            },
        ),
        (
            500,
            "failed",
            ProviderError::Http {
                status: 500,
                body: "failed".into(),
            },
        ),
        (
            429,
            "limited",
            ProviderError::RateLimited {
                retry_after: Some(Duration::from_secs(7)),
            },
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("Retry-After", "7")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;
        // When: discovering models from a failing endpoint.
        let result = providers::list_models(&server.uri(), &ProviderAuth::new("key")).await;
        // Then: shared HTTP error mapping is preserved.
        assert_eq!(result.unwrap_err(), expected);
    }
    for body in ["not-json", r#"{"data":[{}]}"#, r#"{"data":"wrong"}"#] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        // When: decoding an invalid response.
        let result = providers::list_models(&server.uri(), &ProviderAuth::new("key")).await;
        // Then: decode failures are JSON errors, not transport errors.
        assert!(matches!(result, Err(ProviderError::InvalidJson { .. })));
    }
}

#[tokio::test]
async fn returns_transport_error_when_connection_is_refused() {
    // Given: a reserved port with no listening socket.
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let base_url = format!("http://{}", socket.local_addr().unwrap());
    // When: connecting to that port.
    let result = providers::list_models(&base_url, &ProviderAuth::new("key")).await;
    // Then: the connection error uses the transport variant.
    assert!(matches!(result, Err(ProviderError::Transport { .. })));
}
