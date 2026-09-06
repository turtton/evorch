use std::sync::Arc;
use std::time::Duration;

use event_bus::EventBus;
use futures_util::StreamExt;
use mock_openai::{ScriptedResponse, StreamingMockOpenAi, WriteMode};
use providers::provider::openai_compatible::OpenAiCompatibleClient;
use providers::{
    ChatRequest, ChatResponse, ContentBlock, FinishReason, Message, ProviderAuth, ProviderClient,
    Role, StreamEvent, Usage,
};
use serde_json::json;

fn request() -> ChatRequest {
    ChatRequest {
        model: "test-model".to_string(),
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

async fn collect_events(server: &StreamingMockOpenAi) -> Vec<StreamEvent> {
    let bus = Arc::new(EventBus::new(16));
    let client = OpenAiCompatibleClient::new(
        server.base_url(),
        "test-compatible",
        Duration::from_secs(1),
        Some(bus),
    )
    .expect("compatible client can be constructed");

    client
        .stream(&ProviderAuth::new("sk-compatible"), &request())
        .await
        .expect("stream can be started")
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("stream succeeds")
}

#[tokio::test(flavor = "multi_thread")]
async fn client_stream_text_yields_text_deltas_then_completed() {
    // Given: three text fragments with default zero usage.
    let server = StreamingMockOpenAi::spawn(vec![ScriptedResponse::text_stream(
        "chatcmpl-1",
        "test-model",
        ["Hel", "lo", " world"],
    )]);

    // When: the real client consumes the HTTP SSE response to completion.
    let events = collect_events(&server).await;

    // Then: exactly three deltas precede one complete, accumulated response.
    assert_eq!(
        events,
        vec![
            StreamEvent::TextDelta { text: "Hel".into() },
            StreamEvent::TextDelta { text: "lo".into() },
            StreamEvent::TextDelta {
                text: " world".into(),
            },
            StreamEvent::Completed {
                response: ChatResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::Text {
                            text: "Hello world".into(),
                        }],
                    },
                    usage: Usage {
                        input_tokens: 0,
                        output_tokens: 0,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0,
                    },
                    finish_reason: FinishReason::Stop,
                },
            },
        ]
    );
    let recorded = server.recorded_requests();
    let [sent] = recorded.as_slice() else {
        panic!("expected one recorded request: {recorded:?}");
    };
    assert!(sent.stream);
    assert_eq!(sent.body["stream_options"]["include_usage"], true);
    assert_eq!(sent.authorization.as_deref(), Some("Bearer sk-compatible"));
    assert_eq!(sent.path, "/v1/chat/completions");
}

#[tokio::test(flavor = "multi_thread")]
async fn client_stream_tool_call_yields_tool_call_deltas_and_tool_use_block() {
    // Given: one tool call whose JSON input spans two fragments.
    let server = StreamingMockOpenAi::spawn(vec![ScriptedResponse::tool_call(
        "chatcmpl-2",
        "test-model",
        0,
        "call_1",
        "edit",
        ["{\"path\":\"a.txt\",", "\"content\":\"x\"}"],
    )]);

    // When: the real client consumes the tool-call SSE response.
    let events = collect_events(&server).await;

    // Then: both verbatim argument deltas precede exactly one parsed tool use.
    assert_eq!(
        events,
        vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("edit".into()),
                arguments_delta: "{\"path\":\"a.txt\",".into(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: "\"content\":\"x\"}".into(),
            },
            StreamEvent::Completed {
                response: ChatResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::ToolUse {
                            id: "call_1".into(),
                            name: "edit".into(),
                            input: json!({"path": "a.txt", "content": "x"}),
                        }],
                    },
                    usage: Usage::default(),
                    finish_reason: FinishReason::ToolUse,
                },
            },
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn client_stream_frame_per_write_and_whole_body_agree() {
    // Given: identical text scripts served with distinct SSE write strategies.
    let script = ScriptedResponse::text_stream("chatcmpl-1", "test-model", ["Hel", "lo", " world"]);
    let framed = StreamingMockOpenAi::spawn_with(vec![script.clone()], WriteMode::FramePerWrite);
    let whole = StreamingMockOpenAi::spawn_with(vec![script], WriteMode::WholeBody);

    // When: the real client consumes both responses.
    let framed_events = collect_events(&framed).await;
    let whole_events = collect_events(&whole).await;

    // Then: server write strategy does not change the canonical event vector.
    assert_eq!(framed_events, whole_events);
}
