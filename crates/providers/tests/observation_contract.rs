// allow: SIZE_OK — 観測イベントの契約表を1つの統合テストバイナリに集約する。
mod support;

use std::sync::Arc;
use std::time::Duration;

use event_bus::{Event, EventBus, EventKind, ProviderEvent, ProviderFailureKind, UsageEvent};
use futures_util::StreamExt;
use providers::provider::anthropic::{AnthropicClient, AnthropicConfig};
use providers::provider::openai::{OpenAiClient, OpenAiConfig};
use providers::provider::openai_compatible::OpenAiCompatibleClient;
use providers::{
    ChatRequest, ContentBlock, Message, ProviderAuth, ProviderClient, ProviderError, Role,
};
use support::{
    fixture, json_response, next_event, next_provider_event, next_usage_event, sse_response,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const OPENAI_MODEL: &str = "gpt-contract";
const ANTHROPIC_MODEL: &str = "claude-test";
const COMPATIBLE_MODEL: &str = "compatible-model";

fn request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.to_string(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Hello".to_string(),
            }],
        }],
        tools: Vec::new(),
        temperature: None,
        max_tokens: None,
    }
}

async fn mount(server: &MockServer, endpoint: &str, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path(endpoint))
        .respond_with(response)
        .mount(server)
        .await;
}

async fn collect_events(rx: &mut event_bus::EventReceiver, count: usize) -> Vec<Event> {
    let mut events = Vec::with_capacity(count);
    for _ in 0..count {
        events.push(next_event(rx).await);
    }
    events
}

async fn assert_no_more_events(rx: &mut event_bus::EventReceiver) {
    assert!(
        tokio::time::timeout(Duration::from_millis(30), rx.recv())
            .await
            .is_err()
    );
}

fn started(event: &Event) -> (&str, &str, Option<&str>, &str, &str, bool) {
    let EventKind::Provider(ProviderEvent::RequestStarted {
        request_id,
        provider,
        profile,
        protocol,
        model,
        streaming,
    }) = &event.kind
    else {
        panic!("RequestStarted を期待しました: {:?}", event.kind)
    };
    (
        request_id,
        provider,
        profile.as_deref(),
        protocol,
        model,
        *streaming,
    )
}

fn failed(event: &Event) -> (&str, ProviderFailureKind, u64) {
    let EventKind::Provider(ProviderEvent::RequestFailed {
        request_id,
        failure,
        duration_ms,
        ..
    }) = &event.kind
    else {
        panic!("RequestFailed を期待しました: {:?}", event.kind)
    };
    (request_id, *failure, *duration_ms)
}

fn openai_client(server: &MockServer, bus: Arc<EventBus>, timeout: Duration) -> OpenAiClient {
    OpenAiClient::new(OpenAiConfig {
        base_url: server.uri(),
        timeout,
        event_bus: Some(bus),
    })
    .expect("OpenAI client を構築できる")
    .with_profile("openai-profile")
}

fn anthropic_client(server: &MockServer, bus: Arc<EventBus>) -> AnthropicClient {
    AnthropicClient::new(AnthropicConfig {
        base_url: server.uri(),
        timeout: Duration::from_secs(1),
        event_bus: Some(bus),
    })
    .expect("Anthropic client を構築できる")
    .with_profile("anthropic-profile")
}

fn compatible_client(server: &MockServer, bus: Arc<EventBus>) -> OpenAiCompatibleClient {
    OpenAiCompatibleClient::new(
        server.uri(),
        "custom-compatible",
        Duration::from_secs(1),
        Some(bus),
    )
    .expect("互換 client を構築できる")
    .with_profile("compatible-profile")
}

fn assert_success_sequence(
    events: &[Event],
    provider: &str,
    profile: &str,
    protocol: &str,
    model: &str,
    streaming: bool,
    expects_first_token: bool,
) {
    let (
        request_id,
        actual_provider,
        actual_profile,
        actual_protocol,
        actual_model,
        actual_streaming,
    ) = started(&events[0]);
    assert_eq!(actual_provider, provider);
    assert_eq!(actual_profile, Some(profile));
    assert_eq!(actual_protocol, protocol);
    assert_eq!(actual_model, model);
    assert_eq!(actual_streaming, streaming);
    let usage_index = if expects_first_token {
        assert!(matches!(
            &events[1].kind,
            EventKind::Provider(ProviderEvent::FirstTokenObserved { request_id: id, .. }) if id == request_id
        ));
        2
    } else {
        1
    };
    let EventKind::Usage(UsageEvent::Usage {
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        ..
    }) = events[usage_index].kind
    else {
        panic!("Usage を期待しました")
    };
    let EventKind::Provider(ProviderEvent::RequestCompleted {
        request_id: completed_id,
        duration_ms,
        input_tokens: completed_input,
        output_tokens: completed_output,
        cache_read_tokens: completed_cache_read,
        cache_write_tokens: completed_cache_write,
        finish_reason,
        ..
    }) = &events[usage_index + 1].kind
    else {
        panic!("RequestCompleted を期待しました")
    };
    assert_eq!(completed_id, request_id);
    assert_eq!(
        (
            *completed_input,
            *completed_output,
            *completed_cache_read,
            *completed_cache_write
        ),
        (
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens
        )
    );
    assert_eq!(finish_reason, "stop");
    assert!(*duration_ms < 2_000);
}

// Given: EventBus 接続済み OpenAI client と正常 JSON / When: send / Then: Started→Usage→Completed の順で同一 attempt を観測する
#[tokio::test(flavor = "multi_thread")]
async fn openai_send_success_emits_ordered_observation() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/chat/completions",
        json_response(200, &fixture("openai", "send_text.json"))
            .set_delay(Duration::from_millis(2)),
    )
    .await;
    let bus = Arc::new(EventBus::new(16));
    let mut rx = bus.subscribe();

    openai_client(&server, bus, Duration::from_secs(1))
        .send(&ProviderAuth::new("sk"), &request(OPENAI_MODEL))
        .await
        .expect("send は成功する");

    let ProviderEvent::RequestStarted {
        request_id,
        provider,
        profile,
        protocol,
        model,
        streaming,
    } = next_provider_event(&mut rx).await
    else {
        panic!("RequestStarted を期待")
    };
    assert_eq!(provider, "openai");
    assert_eq!(profile.as_deref(), Some("openai-profile"));
    assert_eq!(protocol, "openai-chat-completions");
    assert_eq!(model, OPENAI_MODEL);
    assert!(!streaming);
    let usage = next_usage_event(&mut rx).await;
    let ProviderEvent::RequestCompleted {
        request_id: completed_id,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        finish_reason,
        duration_ms,
        ..
    } = next_provider_event(&mut rx).await
    else {
        panic!("RequestCompleted を期待")
    };
    assert_eq!(completed_id, request_id);
    let UsageEvent::Usage {
        input_tokens: usage_input,
        output_tokens: usage_output,
        cache_read_tokens: usage_cache_read,
        cache_write_tokens: usage_cache_write,
        ..
    } = usage
    else {
        panic!("Usage を期待")
    };
    assert_eq!(
        (
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens
        ),
        (
            usage_input,
            usage_output,
            usage_cache_read,
            usage_cache_write
        )
    );
    assert_eq!(finish_reason, "stop");
    assert!(duration_ms > 0);
}

// Given: EventBus 接続済み OpenAI client と正常 SSE / When: stream を完了まで読む / Then: Started→FirstToken 1件→Usage→Completed を観測する
#[tokio::test(flavor = "multi_thread")]
async fn openai_stream_success_emits_first_token_once() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/chat/completions",
        sse_response(&fixture("openai", "stream_text.sse")),
    )
    .await;
    let bus = Arc::new(EventBus::new(16));
    let mut rx = bus.subscribe();

    let _: Vec<_> = openai_client(&server, bus, Duration::from_secs(1))
        .stream(&ProviderAuth::new("sk"), &request(OPENAI_MODEL))
        .await
        .expect("stream を開始できる")
        .collect()
        .await;

    let events = collect_events(&mut rx, 4).await;
    assert_success_sequence(
        &events,
        "openai",
        "openai-profile",
        "openai-chat-completions",
        OPENAI_MODEL,
        true,
        true,
    );
    assert_no_more_events(&mut rx).await;
}

async fn assert_openai_send_failure(response: ResponseTemplate, expected: ProviderFailureKind) {
    let server = MockServer::start().await;
    mount(&server, "/chat/completions", response).await;
    let bus = Arc::new(EventBus::new(16));
    let mut rx = bus.subscribe();

    openai_client(&server, bus, Duration::from_secs(1))
        .send(&ProviderAuth::new("sk"), &request(OPENAI_MODEL))
        .await
        .expect_err("send は失敗する");

    let events = collect_events(&mut rx, 2).await;
    let (request_id, ..) = started(&events[0]);
    let (failed_id, failure, _) = failed(&events[1]);
    assert_eq!(failed_id, request_id);
    assert_eq!(failure, expected);
    assert_no_more_events(&mut rx).await;
}

// Given: OpenAI HTTP 500 / When: send / Then: Started→Http failure を観測する
#[tokio::test(flavor = "multi_thread")]
async fn openai_send_http_500_emits_http_failure() {
    assert_openai_send_failure(
        json_response(500, "boom"),
        ProviderFailureKind::Http { status: 500 },
    )
    .await;
}

// Given: OpenAI 200 と不正 JSON / When: send / Then: Started→InvalidResponse を観測する
#[tokio::test(flavor = "multi_thread")]
async fn openai_send_invalid_json_emits_invalid_response() {
    assert_openai_send_failure(
        json_response(200, "{broken"),
        ProviderFailureKind::InvalidResponse,
    )
    .await;
}

// Given: OpenAI stream の不正 JSON frame / When: stream を読む / Then: Started→InvalidResponse を観測する
#[tokio::test(flavor = "multi_thread")]
async fn openai_stream_invalid_json_emits_invalid_response() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/chat/completions",
        sse_response("data: {invalid json}\n\n"),
    )
    .await;
    let bus = Arc::new(EventBus::new(16));
    let mut rx = bus.subscribe();
    let events = openai_client(&server, bus, Duration::from_secs(1))
        .stream(&ProviderAuth::new("sk"), &request(OPENAI_MODEL))
        .await
        .expect("stream を開始できる")
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        events.as_slice(),
        [Err(ProviderError::InvalidJson { .. })]
    ));

    let observed = collect_events(&mut rx, 2).await;
    assert_eq!(failed(&observed[1]).1, ProviderFailureKind::InvalidResponse);
    assert_eq!(failed(&observed[1]).0, started(&observed[0]).0);
}

// Given: 100ms timeout と 2秒遅延する OpenAI 応答 / When: send / Then: Started→Timeout を短い duration で観測する
#[tokio::test(flavor = "multi_thread")]
async fn openai_send_timeout_emits_timeout_failure() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/chat/completions",
        json_response(200, &fixture("openai", "send_text.json")).set_delay(Duration::from_secs(2)),
    )
    .await;
    let bus = Arc::new(EventBus::new(16));
    let mut rx = bus.subscribe();
    openai_client(&server, bus, Duration::from_millis(100))
        .send(&ProviderAuth::new("sk"), &request(OPENAI_MODEL))
        .await
        .expect_err("timeout する");

    let observed = collect_events(&mut rx, 2).await;
    let (failed_id, failure, duration_ms) = failed(&observed[1]);
    assert_eq!(failed_id, started(&observed[0]).0);
    assert_eq!(failure, ProviderFailureKind::Timeout);
    assert!(duration_ms < 2_000);
}

// Given: Anthropic 正常 JSON / When: send / Then: Started→Usage→Completed を観測する
#[tokio::test(flavor = "multi_thread")]
async fn anthropic_send_success_emits_ordered_observation() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/messages",
        json_response(200, &fixture("anthropic", "send_text.json")),
    )
    .await;
    let bus = Arc::new(EventBus::new(16));
    let mut rx = bus.subscribe();
    anthropic_client(&server, bus)
        .send(&ProviderAuth::new("sk"), &request(ANTHROPIC_MODEL))
        .await
        .expect("send は成功する");

    let events = collect_events(&mut rx, 3).await;
    assert_success_sequence(
        &events,
        "anthropic",
        "anthropic-profile",
        "anthropic-messages",
        ANTHROPIC_MODEL,
        false,
        false,
    );
}

// Given: Anthropic 正常 SSE / When: stream を完了まで読む / Then: Started→FirstToken→Usage→Completed を観測する
#[tokio::test(flavor = "multi_thread")]
async fn anthropic_stream_success_emits_ordered_observation() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/messages",
        sse_response(&fixture("anthropic", "stream_text.sse")),
    )
    .await;
    let bus = Arc::new(EventBus::new(16));
    let mut rx = bus.subscribe();
    let _: Vec<_> = anthropic_client(&server, bus)
        .stream(&ProviderAuth::new("sk"), &request(ANTHROPIC_MODEL))
        .await
        .expect("stream を開始できる")
        .collect()
        .await;

    let events = collect_events(&mut rx, 4).await;
    assert_success_sequence(
        &events,
        "anthropic",
        "anthropic-profile",
        "anthropic-messages",
        ANTHROPIC_MODEL,
        true,
        true,
    );
}

async fn assert_anthropic_failure(
    response: ResponseTemplate,
    stream: bool,
    expected: ProviderFailureKind,
) {
    let server = MockServer::start().await;
    mount(&server, "/messages", response).await;
    let bus = Arc::new(EventBus::new(16));
    let mut rx = bus.subscribe();
    let client = anthropic_client(&server, bus);
    if stream {
        let _: Vec<_> = client
            .stream(&ProviderAuth::new("sk"), &request(ANTHROPIC_MODEL))
            .await
            .expect("stream は開始する")
            .collect()
            .await;
    } else {
        client
            .send(&ProviderAuth::new("sk"), &request(ANTHROPIC_MODEL))
            .await
            .expect_err("send は失敗する");
    }
    let events = collect_events(&mut rx, 2).await;
    assert_eq!(failed(&events[1]).0, started(&events[0]).0);
    assert_eq!(failed(&events[1]).1, expected);
}

// Given: Anthropic HTTP 500 / When: send / Then: Http failure を観測する
#[tokio::test(flavor = "multi_thread")]
async fn anthropic_send_http_500_emits_http_failure() {
    assert_anthropic_failure(
        json_response(500, "boom"),
        false,
        ProviderFailureKind::Http { status: 500 },
    )
    .await;
}

// Given: Anthropic 不正 JSON / When: send / Then: InvalidResponse を観測する
#[tokio::test(flavor = "multi_thread")]
async fn anthropic_send_invalid_json_emits_invalid_response() {
    assert_anthropic_failure(
        json_response(200, "{broken"),
        false,
        ProviderFailureKind::InvalidResponse,
    )
    .await;
}

// Given: Anthropic 不正 SSE JSON / When: stream / Then: InvalidResponse を観測する
#[tokio::test(flavor = "multi_thread")]
async fn anthropic_stream_invalid_json_emits_invalid_response() {
    assert_anthropic_failure(
        sse_response("event: content_block_delta\ndata: {broken\n\n"),
        true,
        ProviderFailureKind::InvalidResponse,
    )
    .await;
}

// Given: OpenAI互換正常 JSON / When: send / Then: custom provider label で Started→Usage→Completed を観測する
#[tokio::test(flavor = "multi_thread")]
async fn compatible_send_success_emits_custom_provider_observation() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/chat/completions",
        json_response(200, &fixture("openai", "send_text.json")),
    )
    .await;
    let bus = Arc::new(EventBus::new(16));
    let mut rx = bus.subscribe();
    compatible_client(&server, bus)
        .send(&ProviderAuth::new("sk"), &request(COMPATIBLE_MODEL))
        .await
        .expect("send は成功する");

    let events = collect_events(&mut rx, 3).await;
    assert_success_sequence(
        &events,
        "custom-compatible",
        "compatible-profile",
        "openai-chat-completions",
        COMPATIBLE_MODEL,
        false,
        false,
    );
}

// Given: OpenAI互換正常 SSE / When: stream / Then: FirstToken を含む成功列を観測する
#[tokio::test(flavor = "multi_thread")]
async fn compatible_stream_success_emits_observation() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/chat/completions",
        sse_response(&fixture("openai", "stream_text.sse")),
    )
    .await;
    let bus = Arc::new(EventBus::new(16));
    let mut rx = bus.subscribe();
    let _: Vec<_> = compatible_client(&server, bus)
        .stream(&ProviderAuth::new("sk"), &request(COMPATIBLE_MODEL))
        .await
        .expect("stream を開始できる")
        .collect()
        .await;

    let events = collect_events(&mut rx, 4).await;
    assert_success_sequence(
        &events,
        "custom-compatible",
        "compatible-profile",
        "openai-chat-completions",
        COMPATIBLE_MODEL,
        true,
        true,
    );
}

// Given: OpenAI互換 HTTP 500 / When: send / Then: custom provider label の Http failure を観測する
#[tokio::test(flavor = "multi_thread")]
async fn compatible_send_http_500_emits_failure() {
    let server = MockServer::start().await;
    mount(&server, "/chat/completions", json_response(500, "boom")).await;
    let bus = Arc::new(EventBus::new(16));
    let mut rx = bus.subscribe();
    compatible_client(&server, bus)
        .send(&ProviderAuth::new("sk"), &request(COMPATIBLE_MODEL))
        .await
        .expect_err("send は失敗する");

    let events = collect_events(&mut rx, 2).await;
    assert_eq!(failed(&events[1]).0, started(&events[0]).0);
    assert_eq!(
        failed(&events[1]).1,
        ProviderFailureKind::Http { status: 500 }
    );
}

// Given: OpenAI stream の最初の delta / When: consumer が stream を破棄 / Then: Started と同じ request ID の Other failure を観測する
#[tokio::test(flavor = "multi_thread")]
async fn stream_consumer_drop_emits_other_failure() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/chat/completions",
        sse_response(&fixture("openai", "stream_text.sse")),
    )
    .await;
    let bus = Arc::new(EventBus::new(16));
    let mut rx = bus.subscribe();
    let mut stream = openai_client(&server, bus, Duration::from_secs(1))
        .stream(&ProviderAuth::new("sk"), &request(OPENAI_MODEL))
        .await
        .expect("stream を開始できる");
    let _ = stream.next().await;
    drop(stream);

    let events = collect_events(&mut rx, 3).await;
    let request_id = started(&events[0]).0;
    assert!(matches!(
        events[1].kind,
        EventKind::Provider(ProviderEvent::FirstTokenObserved { .. })
    ));
    assert_eq!(failed(&events[2]).0, request_id);
    assert_eq!(failed(&events[2]).1, ProviderFailureKind::Other);
}
