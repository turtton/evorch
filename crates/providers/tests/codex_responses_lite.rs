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

use event_bus::{DiagnosticEvent, EventBus, EventKind, ProviderEvent};
use futures_util::StreamExt;
use providers::provider::codex::tokens::CodexTokenStore;
use providers::provider::codex::{CodexClient, CodexConfig};
use providers::{CODEX_CLIENT_VERSION, ChatRequest, ProviderAuth, ProviderClient, ServiceTier};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use contract_support::seeded_store;

const CATALOG: &str = include_str!("../fixtures/codex/models_catalog_0166.json");
const SUCCESS_SSE: &str = include_str!("../fixtures/codex/responses_success.sse");
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
    client_at(&server.uri(), server, store, bus)
}

fn client_at(
    base_url: &str,
    server: &MockServer,
    store: Arc<dyn CodexTokenStore>,
    bus: Option<Arc<EventBus>>,
) -> CodexClient {
    CodexClient::with_config(
        CodexConfig {
            base_url: base_url.to_owned(),
            auth_base_url: server.uri(),
            event_bus: bus,
            ..CodexConfig::default()
        },
        store,
    )
    .map(|client| {
        client.with_catalog_cache(std::sync::Arc::new(providers::CodexCatalogCache::new()))
    })
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

// Discovery failures do not freeze a client's wire capabilities.
#[tokio::test]
async fn model_listing_retries_failure_and_empty_catalog_before_first_wire_use() {
    for response in [
        ResponseTemplate::new(500),
        ResponseTemplate::new(200).set_body_json(json!({"models":[]})),
    ] {
        let server = MockServer::start().await;
        mount_catalog(&server, response).await;
        let client = client(&server, seeded_store(), None);
        let initial = client.list_models(&ProviderAuth::new("")).await;
        assert!(initial.is_err() || initial.unwrap().unwrap().is_empty());
        assert_eq!(catalog_reads(&server).await.len(), 1);
        server.reset().await;
        mount_catalog(
            &server,
            ResponseTemplate::new(200).set_body_raw(CATALOG, "application/json"),
        )
        .await;
        mount_responses(&server).await;
        assert!(
            !client
                .list_models(&ProviderAuth::new(""))
                .await
                .unwrap()
                .unwrap()
                .is_empty()
        );
        client
            .send(
                &ProviderAuth::new(""),
                &chat("gpt-6-astra", "after recovery"),
            )
            .await
            .unwrap();
        assert_eq!(catalog_reads(&server).await.len(), 1);
        assert_eq!(
            codex_support::response_requests(&server).await[0].headers[LITE_HEADER],
            "true"
        );
    }
}

// Wire fallback is immutable even if a later independent listing recovers.
#[tokio::test]
async fn failed_wire_catalog_stays_legacy_after_listing_recovers() {
    use std::sync::atomic::{AtomicU64, Ordering};
    for (response, diagnostics) in [
        (ResponseTemplate::new(500), 3),
        (
            ResponseTemplate::new(200).set_body_json(json!({"models":[]})),
            2,
        ),
    ] {
        let server = MockServer::start().await;
        mount_catalog(&server, response).await;
        mount_responses(&server).await;
        let bus = Arc::new(EventBus::new(64));
        let mut receiver = bus.subscribe();
        let ticks = Arc::new(AtomicU64::new(0));
        let clock = ticks.clone();
        let cache = Arc::new(providers::CodexCatalogCache::with_clock(Arc::new(
            move || std::time::Duration::from_secs(clock.load(Ordering::Relaxed)),
        )));
        let client = client(&server, seeded_store(), Some(bus)).with_catalog_cache(cache);
        for user in ["first", "second"] {
            client
                .send(&ProviderAuth::new(""), &chat("gpt-6-astra", user))
                .await
                .unwrap();
        }
        assert_eq!(
            catalog_reads(&server).await.len(),
            1,
            "send alone never retries"
        );
        server.reset().await;
        mount_catalog(
            &server,
            ResponseTemplate::new(200).set_body_raw(CATALOG, "application/json"),
        )
        .await;
        mount_responses(&server).await;
        assert!(
            !client
                .list_models(&ProviderAuth::new(""))
                .await
                .unwrap()
                .unwrap()
                .is_empty()
        );
        ticks.store(300, Ordering::Relaxed);
        // Once discovery succeeds, its list remains fixed too, even after shared TTL expiry.
        assert!(
            !client
                .list_models(&ProviderAuth::new(""))
                .await
                .unwrap()
                .unwrap()
                .is_empty()
        );
        client
            .send(&ProviderAuth::new(""), &chat("gpt-6-astra", "third"))
            .await
            .unwrap();
        assert_eq!(catalog_reads(&server).await.len(), 1);
        let sent = codex_support::response_requests(&server).await;
        assert!(sent[0].headers.get(LITE_HEADER).is_none());
        let body = body(&sent[0]);
        assert_eq!(body["tools"].as_array().unwrap().len(), 2);
        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(
            body["reasoning"],
            json!({"effort":"medium","summary":"auto"})
        );
        assert!(body.get("text").is_none());
        // A successful discovery remains observable (and failure, if any, is emitted once).
        assert_eq!(catalog_diagnostics(&mut receiver), diagnostics);
    }
}

// Given: one client / When: it lists models and then sends (and the reverse)
// Then: both share a single catalog read
#[tokio::test]
async fn first_nonempty_listing_and_first_wire_capabilities_agree() {
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

async fn priority_exchange(catalog: ResponseTemplate, model: &str) -> (Option<Value>, Value) {
    let server = MockServer::start().await;
    mount_catalog(&server, catalog).await;
    mount_responses(&server).await;
    let bus = Arc::new(EventBus::new(64));
    let mut receiver = bus.subscribe();
    let mut request = chat(model, "hi");
    request.service_tier = Some(ServiceTier::Priority);

    client(&server, seeded_store(), Some(bus))
        .send(&ProviderAuth::new(""), &request)
        .await
        .expect("send succeeds");

    let sent = body(&codex_support::response_requests(&server).await[0]);
    let observed = receiver
        .drain_pending_snapshot()
        .into_iter()
        .find_map(|event| match event.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted { service_tier, .. }) => {
                Some(json!(service_tier))
            }
            _ => None,
        })
        .expect("the request completed");
    (sent.get("service_tier").cloned(), observed)
}

// Given: priority requested for a model whose catalog entry was fetched
// When: the entry's service_tiers are missing, empty, other, fast-only, malformed or advertise
// priority (next to a healthy Lite model in the same catalog)
// Then: priority reaches the wire only for an advertised one, and the observation records
// the tier that was actually sent
#[tokio::test]
async fn priority_is_sent_only_when_the_fetched_entry_advertises_it() {
    for (entry, advertised) in [
        (json!({}), false),
        (json!({"service_tiers": []}), false),
        (json!({"service_tiers": [{"id": "flex"}]}), false),
        (json!({"additional_speed_tiers": ["fast"]}), false),
        (
            json!({"service_tiers": [], "additional_speed_tiers": ["fast"]}),
            false,
        ),
        (json!({"service_tiers": [{}]}), false),
        (json!({"service_tiers": [{"id": "priority"}]}), true),
    ] {
        let mut entry = entry;
        entry["slug"] = json!("m");
        let catalog = json!({"models": [
            entry,
            {"slug": "gpt-6-astra", "use_responses_lite": true,
                "service_tiers": [{"id": "priority"}]}
        ]});
        let (sent, observed) =
            priority_exchange(ResponseTemplate::new(200).set_body_json(catalog), "m").await;
        let expected = advertised.then(|| json!("priority"));
        assert_eq!(sent, expected, "{entry}");
        assert_eq!(observed, expected.unwrap_or(Value::Null), "{entry}");
    }
}

// Given: no usable catalog entry (model not listed, or the catalog failed)
// When: priority is requested / Then: it is sent as requested, as before the catalog existed
#[tokio::test]
async fn priority_is_sent_when_no_entry_was_fetched() {
    let listed_elsewhere = ResponseTemplate::new(200)
        .set_body_json(json!({"models": [{"slug": "other", "service_tiers": []}]}));
    for catalog in [listed_elsewhere, ResponseTemplate::new(500)] {
        let (sent, observed) = priority_exchange(catalog, "m").await;
        assert_eq!(sent, Some(json!("priority")));
        assert_eq!(observed, json!("priority"));
    }
}

// Given: a base URL configured with the `/responses` suffix
// When: a Lite model is requested / Then: the catalog is read from the same normalized base
// as the responses endpoint, so Lite is applied
#[tokio::test]
async fn catalog_and_responses_share_the_normalized_base_url() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/codex/models"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(CATALOG, "application/json"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(SUCCESS_SSE.to_owned(), "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let base = format!("{}/backend-api/codex/responses", server.uri());

    client_at(&base, &server, seeded_store(), None)
        .send(&ProviderAuth::new(""), &chat("gpt-6-astra", "hi"))
        .await
        .expect("send succeeds");

    let sent = codex_support::response_requests(&server).await;
    assert_eq!(sent[0].headers[LITE_HEADER], "true");
    server.verify().await;
}

// Given: a catalog with a Lite and a regular model / When: asked whether a request may
// forbid tool calls / Then: only the regular model can promise it, and a failed catalog
// read keeps the legacy shape and so the promise
#[tokio::test]
async fn only_a_regular_model_can_forbid_tool_calls() {
    let server = MockServer::start().await;
    mount_catalog(
        &server,
        ResponseTemplate::new(200).set_body_raw(CATALOG, "application/json"),
    )
    .await;
    let client = client(&server, seeded_store(), None);
    assert!(
        !client
            .supports_tool_choice_none(&chat("gpt-6-astra", "hi"))
            .await
    );
    assert!(
        client
            .supports_tool_choice_none(&chat("gpt-5.5", "hi"))
            .await
    );

    let failing = MockServer::start().await;
    mount_catalog(&failing, ResponseTemplate::new(500).set_body_string("down")).await;
    let failed = self::client(&failing, seeded_store(), None);
    assert!(
        failed
            .supports_tool_choice_none(&chat("gpt-6-astra", "hi"))
            .await
    );
}

// A profile label is not catalog identity. Direct API calls (metadata and repeated
// GUI Fetch equivalents) reuse the same cache as concurrent and later clients.
#[tokio::test]
async fn concurrent_clients_direct_api_and_metadata_share_one_catalog_get() {
    let server = MockServer::start().await;
    mount_catalog(
        &server,
        ResponseTemplate::new(200).set_body_raw(CATALOG, "application/json"),
    )
    .await;
    let cache = Arc::new(providers::CodexCatalogCache::new());
    let bus = Arc::new(EventBus::new(32));
    let mut receiver = bus.subscribe();
    let a = client(&server, seeded_store(), Some(bus.clone()))
        .with_catalog_cache(cache.clone())
        .with_profile("alpha");
    let b = client(&server, seeded_store(), Some(bus))
        .with_catalog_cache(cache.clone())
        .with_profile("beta");
    let auth = ProviderAuth::new("");
    let (a, b) = tokio::join!(a.list_models(&auth), b.list_models(&auth));
    assert_eq!(a.unwrap(), b.unwrap());
    let mut diagnostics = Vec::new();
    while diagnostics.len() < 2 {
        let event = receiver.recv().await.unwrap();
        if let EventKind::Diagnostic(event) = event.kind {
            diagnostics.push(event);
        }
    }
    assert_eq!(diagnostics.len(), 2);
    assert_eq!(
        diagnostics
            .iter()
            .filter(|d| d.detail.contains("catalog_cache_hit=false"))
            .count(),
        1
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|d| d.detail.contains("catalog_cache_hit=true"))
            .count(),
        1
    );
    for d in diagnostics {
        for secret in ["access-tok-1", "acc-123", &server.uri()] {
            assert!(!d.detail.contains(secret));
        }
    }
    for _ in 0..2 {
        let models = providers::list_codex_models_with_cache(
            &cache,
            &format!("{}/backend-api/codex", server.uri()),
            &ProviderAuth::new("access-tok-1"),
            "acc-123",
        )
        .await
        .unwrap();
        assert_eq!(models.len(), 3);
    }
    client(&server, seeded_store(), None)
        .with_catalog_cache(cache)
        .list_models(&auth)
        .await
        .unwrap();
    assert_eq!(catalog_reads(&server).await.len(), 1);
}
