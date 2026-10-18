use std::sync::{Arc, Mutex};

use providers::provider::anthropic::{AnthropicClient, AnthropicConfig};
use providers::provider::claude::ClaudeTokenStore;
use providers::{HostedWebSearch, HostedWebSearchError, ProviderAuth, ProviderError};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::support::*;

#[tokio::test]
async fn subscription_search_refreshes_same_account_and_rejects_a_warm_account_switch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(success()))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST")).and(path("/token")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "access_token":"secret-refreshed", "refresh_token":"secret-new-refresh", "expires_in":3600, "token_type":"Bearer"
    }))).expect(1).mount(&server).await;
    let store = store();
    let client = claude(&server, store.clone(), None);
    let auth = ProviderAuth::new("unused-key");
    client.search(&auth, &request()).await.unwrap();
    store
        .save(&bundle("secret-account", "secret-expired", 0))
        .unwrap();
    client.search(&auth, &request()).await.unwrap();
    store
        .save(&bundle("secret-different-account", "secret-expired", 0))
        .unwrap();
    let error = client.search(&auth, &request()).await.unwrap_err();
    assert!(matches!(
        error,
        HostedWebSearchError::Provider(ProviderError::Request(_))
    ));
    assert!(!format!("{error:?} {error}").contains("secret"));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[0].headers["authorization"],
        "Bearer secret-subscription-token"
    );
    assert_eq!(
        requests[2].headers["authorization"],
        "Bearer secret-refreshed"
    );
    let first: Value = requests[0].body_json().unwrap();
    let second: Value = requests[2].body_json().unwrap();
    assert_eq!(first["metadata"], second["metadata"]);
    assert_eq!(
        mock_openai::claude_contract::claude_model_input(&requests[0].body).unwrap(),
        mock_openai::claude_contract::claude_model_input(&requests[2].body).unwrap()
    );
}

#[tokio::test]
async fn refresh_failure_is_sanitized_without_inference_or_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(401).set_body_string("private-token-error"))
        .expect(1)
        .mount(&server)
        .await;
    let store = store();
    store
        .save(&bundle("secret-account", "secret-expired", 0))
        .unwrap();
    let client = claude(&server, store, None);
    let mut request = request();
    request.usage_sink = Some(Arc::new(|_| panic!("refresh failed before inference")));
    let error = client
        .search(&ProviderAuth::new("unused"), &request)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        HostedWebSearchError::Provider(ProviderError::Http { status: 401, .. })
    ));
    assert!(!format!("{error:?} {error}").contains("private"));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn cancellation_during_continuation_retains_only_completed_usage() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (requested, continuation_started) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        read_request(&mut first).await;
        let mut body = completed(json!([call("srv_one")]));
        body["stop_reason"] = json!("pause_turn");
        let body = body.to_string();
        first.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        drop(first);
        let (mut next, _) = listener.accept().await.unwrap();
        read_request(&mut next).await;
        requested.send(()).unwrap();
        let mut byte = [0; 1];
        assert_eq!(next.read(&mut byte).await.unwrap(), 0);
    });
    let client = AnthropicClient::new(AnthropicConfig {
        base_url: format!("http://{address}"),
        ..Default::default()
    })
    .unwrap();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let observed = recorded.clone();
    let mut request = request();
    request.usage_sink = Some(Arc::new(move |usage| observed.lock().unwrap().push(usage)));
    let search = tokio::spawn(async move {
        client
            .search(&ProviderAuth::new("dummy-key"), &request)
            .await
    });
    continuation_started.await.unwrap();
    search.abort();
    assert!(search.await.unwrap_err().is_cancelled());
    server.await.unwrap();
    assert_eq!(*recorded.lock().unwrap(), [usage()]);
}

async fn read_request(socket: &mut tokio::net::TcpStream) {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let read = socket.read(&mut chunk).await.unwrap();
        assert_ne!(read, 0);
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let content_length: usize = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            if bytes.len() >= end + 4 + content_length {
                return;
            }
        }
    }
}
