// allow: SIZE_OK — OpenAI互換 usage exactly-once 契約を 1 バイナリに集約する。
mod support;

use std::sync::Arc;
use std::time::Duration;

use event_bus::{Event, EventBus, EventKind, EventReceiver, ProviderEvent, UsageEvent};
use futures_util::StreamExt;
use providers::provider::openai_compatible::OpenAiCompatibleClient;
use providers::{
    ChatRequest, ContentBlock, FinishReason, Message, ProviderAuth, ProviderClient, ProviderError,
    Role, StreamEvent,
};
use support::{fixture, json_response, next_provider_event, next_usage_event, sse_response};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "compatible-model";

/// usage frame を2回含み、[DONE] 後に追加フレームが続く SSE 本文。
/// 2つ目の usage frame は意図的に同一値 (last-wins を pin しないため)。
const DUPLICATE_USAGE_SSE: &str = r#"data: {"id":"chatcmpl_dup","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}

data: {"id":"chatcmpl_dup","choices":[{"index":0,"delta":{"content":" world"},"finish_reason":"stop"}]}

data: {"id":"chatcmpl_dup","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18,"prompt_tokens_details":{"cached_tokens":3}}}

data: {"id":"chatcmpl_dup","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18,"prompt_tokens_details":{"cached_tokens":3}}}

data: [DONE]

data: {"id":"chatcmpl_dup","choices":[{"index":0,"delta":{"content":"post-done"},"finish_reason":null}]}

data: [DONE]
"#;

/// usage frame を含むが完了シグナル ([DONE]) の無い SSE 本文。
const NO_COMPLETION_SIGNAL_SSE: &str = r#"data: {"id":"chatcmpl_nosignal","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}

data: {"id":"chatcmpl_nosignal","choices":[{"index":0,"delta":{"content":" world"},"finish_reason":null}]}

data: {"id":"chatcmpl_nosignal","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18,"prompt_tokens_details":{"cached_tokens":3}}}

"#;

fn request() -> ChatRequest {
    ChatRequest {
        model: MODEL.to_string(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Hello".into(),
            }],
        }],
        tools: Vec::new(),
        temperature: None,
        max_tokens: None,
        observation: None,
    }
}

/// バスが静止するまでイベントを回収する (発行は同期 broadcast なので 50ms 静止で確定)。
async fn drain_events(rx: &mut EventReceiver) -> Vec<Event> {
    let mut events = Vec::new();
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
        events.push(event);
    }
    events
}

/// 回収済みイベント列から Usage イベントのみ抽出する。
fn usage_events(events: &[Event]) -> Vec<&UsageEvent> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Usage(usage) => Some(usage),
            EventKind::Lifecycle(_)
            | EventKind::Message(_)
            | EventKind::Tool(_)
            | EventKind::Provider(_)
            | EventKind::Fault(_)
            | EventKind::AgentMessage(_)
            | EventKind::Compaction(_)
            | EventKind::Orchestrator(_) => None,
        })
        .collect()
}

/// これ以上イベントが来ないことを検証する (30ms)。
async fn assert_no_more_events(rx: &mut EventReceiver) {
    assert!(
        tokio::time::timeout(Duration::from_millis(30), rx.recv())
            .await
            .is_err(),
        "これ以上イベントは発行されないはずです"
    );
}

/// Usage イベントの共通アサーション: provider / model / トークン内訳 (11/7/3/0)。
fn assert_usage_payload(usage: &UsageEvent) {
    let UsageEvent::Usage {
        provider,
        model,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
    } = usage
    else {
        panic!("Usage イベントを期待しました: {usage:?}");
    };
    assert_eq!(provider, "test-compatible");
    assert_eq!(model, MODEL);
    assert_eq!(
        (
            *input_tokens,
            *output_tokens,
            *cache_read_tokens,
            *cache_write_tokens
        ),
        (11, 7, 3, 0),
        "usage ペイロードはフィクスチャ値 (11/7/cached3/write0) と一致するはずです"
    );
}

// Given: カスタムbase URLとprovider label / When: send / Then: /chat/completionsへ送信し Started→Usage→Completed の順で usage をちょうど1回発行する
#[tokio::test(flavor = "multi_thread")]
async fn send_uses_custom_base_url_and_provider_label() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(json_response(200, &fixture("openai", "send_text.json")))
        .expect(1)
        .mount(&server)
        .await;
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();
    let client = OpenAiCompatibleClient::new(
        server.uri(),
        "test-compatible",
        Duration::from_secs(1),
        Some(bus),
    )
    .expect("互換clientを構築できる");

    let response = client
        .send(&ProviderAuth::new("sk-compatible"), &request())
        .await
        .expect("send は成功する");

    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert!(matches!(
        next_provider_event(&mut receiver).await,
        ProviderEvent::RequestStarted {
            streaming: false,
            ..
        }
    ));
    let usage = next_usage_event(&mut receiver).await;
    assert_usage_payload(&usage);
    assert!(matches!(
        next_provider_event(&mut receiver).await,
        ProviderEvent::RequestCompleted {
            streaming: false,
            ..
        }
    ));
    assert_no_more_events(&mut receiver).await;
}

// Given: OpenAI互換SSE / When: stream / Then: text差分とCompletedを返す
#[tokio::test(flavor = "multi_thread")]
async fn stream_uses_openai_wire_contract() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(&fixture("openai", "stream_text.sse")))
        .mount(&server)
        .await;
    let client = OpenAiCompatibleClient::new(
        server.uri(),
        "test-compatible",
        Duration::from_secs(1),
        None,
    )
    .expect("互換clientを構築できる");

    let events = client
        .stream(&ProviderAuth::new("sk-compatible"), &request())
        .await
        .expect("streamを開始できる")
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("streamは成功する");

    assert!(matches!(events.first(), Some(StreamEvent::TextDelta { text }) if text == "Hello"));
    assert!(
        matches!(events.last(), Some(StreamEvent::Completed { response }) if response.finish_reason == FinishReason::Stop)
    );
}

// Given: OpenAI互換backendの429 / When: send / Then: RateLimitedへ変換する
#[tokio::test(flavor = "multi_thread")]
async fn status_429_maps_to_rate_limited() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(json_response(429, r#"{"error":"limited"}"#))
        .mount(&server)
        .await;
    let client = OpenAiCompatibleClient::new(
        server.uri(),
        "test-compatible",
        Duration::from_secs(1),
        None,
    )
    .expect("互換clientを構築できる");

    let error = client
        .send(&ProviderAuth::new("sk-compatible"), &request())
        .await
        .expect_err("429になる");

    assert_eq!(error, ProviderError::RateLimited { retry_after: None });
}

// Given: EventBus 接続済み互換clientと正常SSE / When: stream を完了まで読む / Then: Usage は1回だけ発行され Started→FirstToken→Usage→Completed を観測する
#[tokio::test(flavor = "multi_thread")]
async fn stream_completed_emits_exactly_one_usage_event() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(&fixture("openai", "stream_text.sse")))
        .mount(&server)
        .await;
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();
    let client = OpenAiCompatibleClient::new(
        server.uri(),
        "test-compatible",
        Duration::from_secs(1),
        Some(bus),
    )
    .expect("互換clientを構築できる");

    let events = client
        .stream(&ProviderAuth::new("sk-compatible"), &request())
        .await
        .expect("streamを開始できる")
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("streamは成功する");
    assert!(matches!(
        events.last(),
        Some(StreamEvent::Completed { response }) if response.finish_reason == FinishReason::Stop
    ));

    let bus_events = drain_events(&mut receiver).await;
    let usages = usage_events(&bus_events);
    assert_eq!(
        usages.len(),
        1,
        "usage イベントはちょうど1回発行されるはずです: {bus_events:?}"
    );
    assert_usage_payload(usages[0]);
    assert_eq!(
        bus_events.len(),
        4,
        "Started/FirstToken/Usage/Completed の4件のはずです: {bus_events:?}"
    );
    assert!(matches!(
        bus_events[0].kind,
        EventKind::Provider(ProviderEvent::RequestStarted {
            streaming: true,
            ..
        })
    ));
    assert!(matches!(
        bus_events[1].kind,
        EventKind::Provider(ProviderEvent::FirstTokenObserved { .. })
    ));
    assert!(matches!(
        bus_events[2].kind,
        EventKind::Usage(UsageEvent::Usage { .. })
    ));
    assert!(matches!(
        bus_events[3].kind,
        EventKind::Provider(ProviderEvent::RequestCompleted {
            streaming: true,
            ..
        })
    ));
    assert_no_more_events(&mut receiver).await;
}

// Given: usage frame が2回と [DONE] 後フレームを含む SSE / When: stream を完了まで読む / Then: Completed は1回で post-done 差分は無く usage も1回だけ発行する
#[tokio::test(flavor = "multi_thread")]
async fn stream_duplicate_usage_and_post_done_frames_emit_single_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(DUPLICATE_USAGE_SSE))
        .mount(&server)
        .await;
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();
    let client = OpenAiCompatibleClient::new(
        server.uri(),
        "test-compatible",
        Duration::from_secs(1),
        Some(bus),
    )
    .expect("互換clientを構築できる");

    let events = client
        .stream(&ProviderAuth::new("sk-compatible"), &request())
        .await
        .expect("streamを開始できる")
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("streamは成功する");

    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, StreamEvent::Completed { .. }))
            .count(),
        1,
        "Completed はちょうど1回のはずです: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::TextDelta { text } if text == "post-done")),
        "[DONE] 後のフレームは破棄されるはずです: {events:?}"
    );

    let bus_events = drain_events(&mut receiver).await;
    let usages = usage_events(&bus_events);
    assert_eq!(
        usages.len(),
        1,
        "usage frame が2回あっても発行は1回のはずです: {bus_events:?}"
    );
    assert_usage_payload(usages[0]);
}

// Given: EventBus 接続済み互換clientと HTTP 500 / When: send / Then: 送信は失敗し usage は発行されない
#[tokio::test(flavor = "multi_thread")]
async fn send_http_500_emits_no_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(json_response(500, "boom"))
        .mount(&server)
        .await;
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();
    let client = OpenAiCompatibleClient::new(
        server.uri(),
        "test-compatible",
        Duration::from_secs(1),
        Some(bus),
    )
    .expect("互換clientを構築できる");

    let error = client
        .send(&ProviderAuth::new("sk-compatible"), &request())
        .await
        .expect_err("send は失敗する");
    assert!(matches!(error, ProviderError::Http { status: 500, .. }));

    let bus_events = drain_events(&mut receiver).await;
    assert!(
        usage_events(&bus_events).is_empty(),
        "失敗時に usage は発行されないはずです: {bus_events:?}"
    );
}

// Given: 100ms timeout と 2秒遅延する応答 / When: send / Then: Timeout で失敗し usage は発行されない
#[tokio::test(flavor = "multi_thread")]
async fn send_timeout_emits_no_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            json_response(200, &fixture("openai", "send_text.json"))
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&server)
        .await;
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();
    let client = OpenAiCompatibleClient::new(
        server.uri(),
        "test-compatible",
        Duration::from_millis(100),
        Some(bus),
    )
    .expect("互換clientを構築できる");

    let error = client
        .send(&ProviderAuth::new("sk-compatible"), &request())
        .await
        .expect_err("timeout する");
    assert_eq!(error, ProviderError::Timeout);

    let bus_events = drain_events(&mut receiver).await;
    assert!(
        usage_events(&bus_events).is_empty(),
        "timeout 時に usage は発行されないはずです: {bus_events:?}"
    );
}

// Given: 200 と不正 JSON 本文 / When: send / Then: InvalidJson で失敗し usage は発行されない
#[tokio::test(flavor = "multi_thread")]
async fn send_invalid_json_emits_no_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(json_response(200, "{broken"))
        .mount(&server)
        .await;
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();
    let client = OpenAiCompatibleClient::new(
        server.uri(),
        "test-compatible",
        Duration::from_secs(1),
        Some(bus),
    )
    .expect("互換clientを構築できる");

    let error = client
        .send(&ProviderAuth::new("sk-compatible"), &request())
        .await
        .expect_err("send は失敗する");
    assert!(matches!(error, ProviderError::InvalidJson { .. }));

    let bus_events = drain_events(&mut receiver).await;
    assert!(
        usage_events(&bus_events).is_empty(),
        "JSON 解析失敗時に usage は発行されないはずです: {bus_events:?}"
    );
}

// Given: EventBus 接続済み互換clientと HTTP 500 / When: stream / Then: stream 開始に失敗し usage は発行されない
#[tokio::test(flavor = "multi_thread")]
async fn stream_http_500_emits_no_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(json_response(500, "boom"))
        .mount(&server)
        .await;
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();
    let client = OpenAiCompatibleClient::new(
        server.uri(),
        "test-compatible",
        Duration::from_secs(1),
        Some(bus),
    )
    .expect("互換clientを構築できる");

    let error = client
        .stream(&ProviderAuth::new("sk-compatible"), &request())
        .await
        .err()
        .expect("stream は失敗する");
    assert!(matches!(error, ProviderError::Http { status: 500, .. }));

    let bus_events = drain_events(&mut receiver).await;
    assert!(
        usage_events(&bus_events).is_empty(),
        "stream 開始失敗時に usage は発行されないはずです: {bus_events:?}"
    );
}

// Given: 不正 JSON frame の SSE / When: stream を読む / Then: InvalidJson のみが流れ usage は発行されない
#[tokio::test(flavor = "multi_thread")]
async fn stream_invalid_json_frame_emits_no_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response("data: {invalid json}\n\n"))
        .mount(&server)
        .await;
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();
    let client = OpenAiCompatibleClient::new(
        server.uri(),
        "test-compatible",
        Duration::from_secs(1),
        Some(bus),
    )
    .expect("互換clientを構築できる");

    let events = client
        .stream(&ProviderAuth::new("sk-compatible"), &request())
        .await
        .expect("streamを開始できる")
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        events.as_slice(),
        [Err(ProviderError::InvalidJson { .. })]
    ));

    let bus_events = drain_events(&mut receiver).await;
    assert!(
        usage_events(&bus_events).is_empty(),
        "不正 frame 時に usage は発行されないはずです: {bus_events:?}"
    );
}

// Given: 不正 UTF-8 を含む SSE 本文 / When: stream を読む / Then: InvalidSse のみが流れ usage は発行されない
#[tokio::test(flavor = "multi_thread")]
async fn stream_invalid_sse_emits_no_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(b"data: \xff\xfe\n\n".to_vec(), "text/event-stream"),
        )
        .mount(&server)
        .await;
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();
    let client = OpenAiCompatibleClient::new(
        server.uri(),
        "test-compatible",
        Duration::from_secs(1),
        Some(bus),
    )
    .expect("互換clientを構築できる");

    let events = client
        .stream(&ProviderAuth::new("sk-compatible"), &request())
        .await
        .expect("streamを開始できる")
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        events.as_slice(),
        [Err(ProviderError::InvalidSse { .. })]
    ));

    let bus_events = drain_events(&mut receiver).await;
    assert!(
        usage_events(&bus_events).is_empty(),
        "不正 SSE 時に usage は発行されないはずです: {bus_events:?}"
    );
}

// Given: usage frame を含むが [DONE] の無い SSE / When: stream を終端まで読む / Then: 差分は流れるが Completed も Err も無く usage は発行されない
#[tokio::test(flavor = "multi_thread")]
async fn stream_without_completion_signal_emits_no_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(NO_COMPLETION_SIGNAL_SSE))
        .mount(&server)
        .await;
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();
    let client = OpenAiCompatibleClient::new(
        server.uri(),
        "test-compatible",
        Duration::from_secs(1),
        Some(bus),
    )
    .expect("互換clientを構築できる");

    let events = client
        .stream(&ProviderAuth::new("sk-compatible"), &request())
        .await
        .expect("streamを開始できる")
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("完了シグナル無しの終端は Err 無しで静かに終わるはずです");
    assert_eq!(events.len(), 2, "差分は2つのはずです: {events:?}");
    assert!(matches!(
        &events[0],
        StreamEvent::TextDelta { text } if text == "Hello"
    ));
    assert!(matches!(
        &events[1],
        StreamEvent::TextDelta { text } if text == " world"
    ));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::Completed { .. })),
        "完了シグナル無しでは Completed は発行されないはずです: {events:?}"
    );

    let bus_events = drain_events(&mut receiver).await;
    assert!(
        bus_events.iter().any(|event| matches!(
            event.kind,
            EventKind::Provider(ProviderEvent::RequestFailed { .. })
        )),
        "中途 EOF は RequestFailed として観測されるはずです: {bus_events:?}"
    );
    assert!(
        usage_events(&bus_events).is_empty(),
        "usage frame を受信しても完了しない限り usage は発行されないはずです: {bus_events:?}"
    );
}

// Given: EventBus 接続済み互換clientと正常SSE / When: 最初の差分だけ受信して stream を破棄 / Then: usage は発行されない
#[tokio::test(flavor = "multi_thread")]
async fn stream_consumer_drop_before_completion_emits_no_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(&fixture("openai", "stream_text.sse")))
        .mount(&server)
        .await;
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();
    let client = OpenAiCompatibleClient::new(
        server.uri(),
        "test-compatible",
        Duration::from_secs(1),
        Some(bus),
    )
    .expect("互換clientを構築できる");

    let mut stream = client
        .stream(&ProviderAuth::new("sk-compatible"), &request())
        .await
        .expect("streamを開始できる");
    let first = stream.next().await;
    assert!(
        matches!(&first, Some(Ok(StreamEvent::TextDelta { text })) if text == "Hello"),
        "最初の差分を受信してから破棄する: {first:?}"
    );
    drop(stream);

    let bus_events = drain_events(&mut receiver).await;
    assert!(
        usage_events(&bus_events).is_empty(),
        "完了前に破棄された場合は usage は発行されないはずです: {bus_events:?}"
    );
}
