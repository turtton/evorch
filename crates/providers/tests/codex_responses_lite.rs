//! The model catalog decides the Codex request shape: Responses Lite models get the Lite
//! body and header, the catalog is read once per client, and a failed read keeps the
//! legacy shape for the client's whole lifetime.

#[allow(dead_code)]
#[path = "support/codex.rs"]
mod codex_support;
#[allow(dead_code)]
#[path = "support/codex_contract.rs"]
mod contract_support;

use std::sync::Arc;

use event_bus::{DiagnosticEvent, EventBus, EventKind};
use futures_util::StreamExt;
use providers::provider::codex::tokens::CodexTokenStore;
use providers::provider::codex::{CodexClient, CodexConfig};
use providers::{CODEX_CLIENT_VERSION, ChatRequest, ProviderAuth, ProviderClient};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use contract_support::seeded_store;

const CATALOG: &str = include_str!("fixtures/codex/models_catalog_0166.json");
const SUCCESS_SSE: &str = include_str!("fixtures/codex/responses_success.sse");
const LITE_HEADER: &str = "x-openai-internal-codex-responses-lite";

fn chat(model: &str, user: &str) -> ChatRequest {
    serde_json::from_value(json!({
        "model": model,
        "messages": [
            {"role": "system", "content": [{"type": "text", "text": "Follow the rules."}]},
            {"role": "user", "content": [{"type": "text", "text": user}]}
        ],
        "tools": [
            {"name": "zeta", "description": "Z tool", "input_schema": {"type": "object"}},
            {"name": "alpha", "description": "A tool", "input_schema": {"type": "object"}}
        ]
    }))
    .unwrap()
}

fn client(
    server: &MockServer,
    store: Arc<dyn CodexTokenStore>,
    bus: Option<Arc<EventBus>>,
) -> CodexClient {
    CodexClient::with_config(
        CodexConfig {
            base_url: server.uri(),
            auth_base_url: server.uri(),
            event_bus: bus,
            ..CodexConfig::default()
        },
        store,
    )
    .expect("Codex client can be built")
}

async fn mount_catalog(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path_regex("/models$"))
        .respond_with(response)
        .expect(1)
        .mount(server)
        .await;
}

async fn mount_responses(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path_regex("/responses$"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(SUCCESS_SSE.to_owned(), "text/event-stream"),
        )
        .mount(server)
        .await;
}

async fn catalog_reads(server: &MockServer) -> Vec<Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.method == wiremock::http::Method::GET)
        .collect()
}

fn body(request: &Request) -> Value {
    serde_json::from_slice(&request.body).unwrap()
}

fn catalog_diagnostics(receiver: &mut event_bus::EventReceiver) -> usize {
    receiver
        .drain_pending_snapshot()
        .into_iter()
        .filter(|event| {
            matches!(&event.kind, EventKind::Diagnostic(DiagnosticEvent { code, .. })
                if code == event_bus::event::diagnostic_codes::PROVIDER_CATALOG_FETCH)
        })
        .count()
}

// Given: a catalog that marks gpt-6-astra as Responses Lite
// When: two requests go through one client
// Then: the catalog is read once (with the client version), and both requests carry the
// Lite header and the identical Lite body: tools inside additional_tools, none top-level
#[tokio::test]
async fn lite_model_sends_header_and_lite_body_with_one_catalog_read() {
    let server = MockServer::start().await;
    mount_catalog(
        &server,
        ResponseTemplate::new(200).set_body_raw(CATALOG, "application/json"),
    )
    .await;
    mount_responses(&server).await;
    let client = client(&server, seeded_store(), None);

    for user in ["first", "second"] {
        client
            .send(&ProviderAuth::new(""), &chat("gpt-6-astra", user))
            .await
            .expect("send succeeds");
    }

    let reads = catalog_reads(&server).await;
    assert_eq!(reads.len(), 1, "the catalog is read once per client");
    assert_eq!(
        reads[0].headers["version"].to_str().unwrap(),
        CODEX_CLIENT_VERSION
    );
    let sent = codex_support::response_requests(&server).await;
    assert_eq!(sent.len(), 2);
    let bodies: Vec<_> = sent.iter().map(body).collect();
    for (request, body) in sent.iter().zip(&bodies) {
        assert_eq!(request.headers[LITE_HEADER], "true");
        assert!(body.get("tools").is_none(), "{body}");
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(
            body["reasoning"],
            json!({"effort": "low", "summary": "auto", "context": "all_turns"})
        );
        assert_eq!(body["text"], json!({"verbosity": "low"}));
        assert_eq!(body["input"][0]["type"], "additional_tools");
        assert_eq!(body["input"][1]["role"], "developer");
    }
    assert_eq!(
        bodies[0]["input"][0], bodies[1]["input"][0],
        "the tools item is identical on every request"
    );
    assert_eq!(bodies[0]["input"][1], bodies[1]["input"][1]);
}

// Given: a catalog whose model is not Lite / When: it is requested
// Then: no Lite header, top-level tools, parallel calls on, and the model's own defaults
#[tokio::test]
async fn non_lite_model_keeps_the_regular_shape() {
    let server = MockServer::start().await;
    mount_catalog(
        &server,
        ResponseTemplate::new(200).set_body_raw(CATALOG, "application/json"),
    )
    .await;
    mount_responses(&server).await;

    client(&server, seeded_store(), None)
        .send(&ProviderAuth::new(""), &chat("gpt-5.5", "hi"))
        .await
        .expect("send succeeds");

    let sent = codex_support::response_requests(&server).await;
    assert!(sent[0].headers.get(LITE_HEADER).is_none());
    let body = body(&sent[0]);
    assert_eq!(body["tools"].as_array().unwrap().len(), 2);
    assert_eq!(body["parallel_tool_calls"], true);
    assert_eq!(
        body["reasoning"],
        json!({"effort": "medium", "summary": "auto"})
    );
    assert_eq!(body["input"][0]["role"], "developer");
}

// Given: a catalog that fails with 500
// When: several requests and a model listing follow
// Then: they keep the legacy shape, the catalog is not read again, and exactly one
// diagnostic reports the failed read
#[tokio::test]
async fn failed_catalog_keeps_the_legacy_shape_and_is_not_read_again() {
    let server = MockServer::start().await;
    mount_catalog(&server, ResponseTemplate::new(500).set_body_string("down")).await;
    mount_responses(&server).await;
    let bus = Arc::new(EventBus::new(64));
    let mut receiver = bus.subscribe();
    let client = client(&server, seeded_store(), Some(bus.clone()));

    for user in ["first", "second"] {
        client
            .send(&ProviderAuth::new(""), &chat("gpt-6-astra", user))
            .await
            .expect("send succeeds");
    }
    let listing = client.list_models(&ProviderAuth::new("")).await;

    assert!(
        listing.is_err(),
        "the cached failure is reported to the listing"
    );
    assert_eq!(catalog_reads(&server).await.len(), 1);
    for request in codex_support::response_requests(&server).await {
        assert!(request.headers.get(LITE_HEADER).is_none());
        let body = body(&request);
        assert_eq!(body["tools"].as_array().unwrap().len(), 2);
        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(
            body["reasoning"],
            json!({"effort": "medium", "summary": "auto"})
        );
        assert!(body.get("text").is_none());
    }
    assert_eq!(catalog_diagnostics(&mut receiver), 1);
}

// Given: one client / When: it lists models and then sends (and the reverse)
// Then: both share a single catalog read
#[tokio::test]
async fn model_listing_and_requests_share_one_catalog_read() {
    for list_first in [true, false] {
        let server = MockServer::start().await;
        mount_catalog(
            &server,
            ResponseTemplate::new(200).set_body_raw(CATALOG, "application/json"),
        )
        .await;
        mount_responses(&server).await;
        let client = client(&server, seeded_store(), None);
        let auth = ProviderAuth::new("");
        let request = chat("gpt-6-astra", "hi");
        let send = || client.send(&auth, &request);

        let listed = if list_first {
            let listed = client.list_models(&ProviderAuth::new("")).await;
            send().await.expect("send succeeds");
            listed
        } else {
            send().await.expect("send succeeds");
            client.list_models(&ProviderAuth::new("")).await
        };

        assert_eq!(
            listed.unwrap().unwrap(),
            ["gpt-6-astra", "gpt-6.1-sol", "gpt-5.5"]
        );
        assert_eq!(
            catalog_reads(&server).await.len(),
            1,
            "list_first={list_first}"
        );
        let sent = codex_support::response_requests(&server).await;
        assert_eq!(sent[0].headers[LITE_HEADER], "true");
    }
}

// Given: a Lite request rejected with 401 / When: credentials are recovered and it is resent
// Then: the resend carries the same Lite header and the identical body, and the catalog is
// not read again
#[tokio::test]
async fn unauthorized_resend_keeps_the_lite_header_and_body() {
    let server = MockServer::start().await;
    mount_catalog(
        &server,
        ResponseTemplate::new(200).set_body_raw(CATALOG, "application/json"),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-new",
            "refresh_token": "refresh-new",
            "id_token": contract_support::refreshed_id_token("acc-123"),
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex("/responses$"))
        .respond_with(ResponseTemplate::new(401).set_body_string("expired"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    mount_responses(&server).await;

    client(&server, seeded_store(), None)
        .stream(&ProviderAuth::new(""), &chat("gpt-6-astra", "hi"))
        .await
        .expect("stream opens")
        .collect::<Vec<_>>()
        .await;

    let sent: Vec<_> = codex_support::response_requests(&server)
        .await
        .into_iter()
        .filter(|request| request.url.path() != "/oauth/token")
        .collect();
    assert_eq!(sent.len(), 2);
    for request in &sent {
        assert_eq!(request.headers[LITE_HEADER], "true");
    }
    assert_eq!(sent[0].body, sent[1].body);
    assert_eq!(catalog_reads(&server).await.len(), 1);
}
