//! Codex backend は terminal response の usage を省略しうる (Codex CLI 0.166.0 は任意として扱う)。
//! 欠落をゼロ使用量として報告せず、使用量・cache 観測を出さずに応答は成功させる。

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use event_bus::{
    DiagnosticEvent, EventBus, EventKind, EventReceiver, ProviderEvent, UsageEvent,
    event::diagnostic_codes,
};
use providers::provider::codex::tokens::{CodexTokenStore, InMemoryTokenStore, TokenBundle};
use providers::provider::codex::{CodexClient, CodexConfig};
use providers::{ChatRequest, ContentBlock, FinishReason, ProviderAuth, ProviderClient, Usage};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn request() -> ChatRequest {
    serde_json::from_value(json!({
        "model": "gpt-5.1-codex",
        "messages": [{"role": "user", "content": [{"type": "text", "text": "Hello"}]}],
        "observation": {"run_id": "usage-missing"}
    }))
    .unwrap()
}

async fn client_for(body: String) -> (MockServer, Arc<EventBus>, CodexClient) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;
    let store = Arc::new(InMemoryTokenStore::new());
    let payload = json!({
        "exp": u64::MAX,
        "https://api.openai.com/auth": {"chatgpt_account_id": "usage-missing"}
    });
    store
        .save(&TokenBundle {
            access_token: "usage-token".into(),
            refresh_token: "usage-refresh".into(),
            id_token: format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(payload.to_string())),
        })
        .unwrap();
    let bus = Arc::new(EventBus::new(64));
    let client = CodexClient::with_config(
        CodexConfig {
            base_url: server.uri(),
            auth_base_url: server.uri(),
            event_bus: Some(bus.clone()),
            ..CodexConfig::default()
        },
        store,
    )
    .unwrap();
    (server, bus, client)
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

#[derive(Default)]
struct Observed {
    usage_events: usize,
    cache_reuse: usize,
    completed: usize,
    /// `usage_reported` carried by the RequestCompleted event.
    completed_reports_usage: Option<bool>,
    diagnostics: Vec<DiagnosticEvent>,
}

/// 終端の RequestCompleted までの観測を集める。診断・usage・cache 観測はそれより前に発行される。
async fn observe_until_completed(receiver: &mut EventReceiver) -> Observed {
    let mut observed = Observed::default();
    loop {
        match receiver.recv().await.expect("bus stays open").kind {
            EventKind::Usage(UsageEvent::Usage { .. }) => observed.usage_events += 1,
            EventKind::Provider(ProviderEvent::CacheReuseObserved { .. }) => {
                observed.cache_reuse += 1;
            }
            EventKind::Diagnostic(diagnostic) => observed.diagnostics.push(diagnostic),
            EventKind::Provider(ProviderEvent::RequestCompleted { usage_reported, .. }) => {
                observed.completed += 1;
                observed.completed_reports_usage = Some(usage_reported);
                return observed;
            }
            _ => {}
        }
    }
}

fn assert_usage_missing_only(observed: &Observed) {
    assert_eq!(observed.usage_events, 0, "no UsageEvent without usage");
    assert_eq!(
        observed.cache_reuse, 0,
        "no cache observation without usage"
    );
    assert_eq!(observed.completed, 1);
    assert_eq!(
        observed.completed_reports_usage,
        Some(false),
        "RequestCompleted must say its zero counts are placeholders"
    );
    assert_eq!(observed.diagnostics.len(), 1, "{:?}", observed.diagnostics);
    assert_eq!(
        observed.diagnostics[0].code,
        diagnostic_codes::PROVIDER_USAGE_MISSING
    );
}

// Given: Codex が usage を null またはキーなしで返す
// When: CodexClient::send / When: Then: Stop で成功し、使用量は 0 のまま未報告診断だけが出る
#[tokio::test]
async fn send_completes_with_stop_when_the_terminal_response_has_no_usage() {
    for response in [
        json!({"id": "resp_1", "usage": null}),
        json!({"id": "resp_1"}),
    ] {
        let body = sse(&[
            json!({"type": "response.output_text.delta", "delta": "hi"}),
            json!({"type": "response.completed", "response": response}),
        ]);
        let (_server, bus, client) = client_for(body).await;
        let mut receiver = bus.subscribe();

        let response = client
            .send(&ProviderAuth::new(""), &request())
            .await
            .unwrap();

        assert_eq!(response.finish_reason, FinishReason::Stop);
        assert_eq!(response.usage, Usage::default());
        assert!(!response.usage_reported);
        assert_eq!(
            response.message.content,
            vec![ContentBlock::Text { text: "hi".into() }]
        );
        assert_usage_missing_only(&observe_until_completed(&mut receiver).await);
    }
}

// Given: usage を報告する通常の response.completed
// When: send / Then: UsageEvent があり、未報告診断は出ない
#[tokio::test]
async fn reported_usage_still_emits_usage_and_no_missing_diagnostic() {
    let body = sse(&[json!({
        "type": "response.completed",
        "response": {"id": "resp_1", "usage": {"input_tokens": 3, "output_tokens": 2}}
    })]);
    let (_server, bus, client) = client_for(body).await;
    let mut receiver = bus.subscribe();

    let response = client
        .send(&ProviderAuth::new(""), &request())
        .await
        .unwrap();

    assert_eq!(response.usage.input_tokens, 3);
    assert!(response.usage_reported);
    let observed = observe_until_completed(&mut receiver).await;
    assert_eq!(observed.usage_events, 1);
    assert_eq!(observed.completed_reports_usage, Some(true));
    assert!(
        observed.diagnostics.is_empty(),
        "{:?}",
        observed.diagnostics
    );
}

// Given: usage を持たない compaction の response.completed
// When: compact / Then: 暗号文は返り、使用量は 0 のまま未報告診断だけが出る
#[tokio::test]
async fn compaction_without_usage_returns_the_blob_and_records_only_a_diagnostic() {
    let body = sse(&[
        json!({"type": "response.output_item.done",
            "item": {"type": "compaction", "encrypted_content": "opaque"}}),
        json!({"type": "response.completed", "response": {"id": "resp_1"}}),
    ]);
    let (_server, bus, client) = client_for(body).await;
    let mut receiver = bus.subscribe();

    let result = client
        .compactor()
        .unwrap()
        .compact(&request())
        .await
        .unwrap();

    assert_eq!(result.encrypted_content, "opaque");
    assert_eq!(result.usage, Usage::default());
    assert!(!result.usage_reported);
    assert_usage_missing_only(&observe_until_completed(&mut receiver).await);
}
