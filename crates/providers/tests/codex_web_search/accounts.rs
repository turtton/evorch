use providers::provider::codex::oauth::CODEX_CLIENT_ID;
use providers::provider::codex::tokens::CodexTokenStore;
use providers::{HostedWebSearch, ProviderAuth, ProviderClient};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::support::*;

#[tokio::test]
async fn search_shares_chat_session_headers_endpoint_and_refresh() {
    let server = MockServer::start().await;
    let store = store("expired-access", "selected-account", 0);
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_json(json!({"client_id":CODEX_CLIENT_ID,"grant_type":"refresh_token","refresh_token":"refresh-old"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token":"refreshed-access", "refresh_token":"refresh-new",
            "id_token":jwt(u64::MAX,"selected-account")
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", "Bearer refreshed-access"))
        .and(header("chatgpt-account-id", "selected-account"))
        .and(header("originator", "codex_cli_rs"))
        .and(header("OpenAI-Beta", "responses=experimental"))
        .and(header("accept", "text/event-stream"))
        .and(header("version", "0.200.0"))
        .and(header("user-agent", "codex_cli_rs/0.200.0"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = request.body_json().unwrap();
            let body = if body["tools"][0]["type"] == "web_search" {
                success()
            } else {
                include_str!("../fixtures/codex/responses_success.sse").into()
            };
            ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
        })
        .expect(2)
        .mount(&server)
        .await;
    let client = client(&server, store.clone(), None);
    let auth = ProviderAuth::new("must-not-be-used");
    let search_request = request();
    let ordinary = chat_request();
    let (search, chat) = tokio::join!(
        client
            .hosted_web_search()
            .unwrap()
            .search(&auth, &search_request),
        client.send(&auth, &ordinary)
    );
    assert!(search.is_ok(), "{search:?}");
    assert!(chat.is_ok(), "{chat:?}");
    assert_eq!(
        store.load().unwrap().unwrap().access_token,
        "refreshed-access"
    );

    let requests = server.received_requests().await.unwrap();
    let responses: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path().ends_with("/responses"))
        .collect();
    assert_eq!(responses.len(), 2);
    assert_eq!(
        responses[0].headers["session-id"],
        responses[1].headers["session-id"]
    );
    assert_ne!(
        responses[0].headers["thread-id"],
        responses[1].headers["thread-id"]
    );
    for response in responses {
        assert_eq!(
            response.headers["thread-id"],
            response.headers["x-client-request-id"]
        );
        let body: Value = response.body_json().unwrap();
        if body["tools"][0]["type"] == "web_search" {
            assert_eq!(
                body["tools"],
                json!([{"type":"web_search","external_web_access":true}])
            );
            assert_eq!(body["model"], "selected-codex-model");
            assert_eq!(body["input"][0]["content"][0]["text"], search_request.query);
            assert_eq!(body["reasoning"]["effort"], "low");
            assert_eq!(body["include"], json!(["web_search_call.action.sources"]));
            assert_eq!(body["tool_choice"], "required");
            assert_eq!(body["store"], false);
            assert_eq!(body["stream"], true);
            assert!(body.get("observation").is_none());
            assert!(body.get("usage_sink").is_none());
        } else {
            assert_eq!(body["tool_choice"], "auto");
            assert_eq!(
                body,
                serde_json::to_value(providers::wire::codex::to_wire_request(&ordinary)).unwrap()
            );
        }
    }
}

#[tokio::test]
async fn independent_clients_keep_selected_accounts_separate() {
    let server = MockServer::start().await;
    for account in ["one", "two"] {
        Mock::given(method("POST"))
            .and(header("authorization", format!("Bearer access-{account}")))
            .and(header("chatgpt-account-id", account))
            .respond_with(ResponseTemplate::new(200).set_body_raw(success(), "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;
    }
    let one = client(&server, store("access-one", "one", u64::MAX), None);
    let two = client(&server, store("access-two", "two", u64::MAX), None);
    let auth = ProviderAuth::new("unused");
    let request = request();
    let (one, two) = tokio::join!(one.search(&auth, &request), two.search(&auth, &request));
    assert!(one.is_ok());
    assert!(two.is_ok());
}
