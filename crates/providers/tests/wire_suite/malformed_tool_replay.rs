//! Outbound repair must not repair execution arguments or lose signed thinking.
use futures_util::StreamExt;
use providers::provider::anthropic::{AnthropicClient, AnthropicConfig};
use providers::{
    ChatRequest, ContentBlock, FinishReason, Message, ProviderAuth, ProviderClient, Role,
    StreamEvent, ToolResultContent,
};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn request() -> ChatRequest {
    ChatRequest {
        model: "claude-test".into(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "go".into() }],
        }],
        tools: vec![],
        tool_choice: None,
        responses_replay: None,
        temperature: None,
        max_tokens: None,
        reasoning_effort: None,
        service_tier: None,
        output_schema: None,
        observation: None,
    }
}
fn response() -> Value {
    json!({"id":"reply","type":"message","role":"assistant","model":"claude-test","content":[{"type":"thinking","thinking":"signed","signature":"sig"},{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":2}})
}
fn legacy() -> Vec<Message> {
    let path = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("fixtures/anthropic/legacy_null_history.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}
fn body(request: &wiremock::Request) -> Value {
    serde_json::from_slice(&request.body).unwrap()
}
fn assert_repaired(body: &Value) {
    let calls: Vec<_> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["content"].as_array().unwrap())
        .filter(|b| b["type"] == "tool_use")
        .collect();
    assert_eq!(
        calls.len(),
        1,
        "raw/projection must not duplicate a tool call"
    );
    assert_eq!(calls[0]["input"], json!({}));
    assert_eq!(calls[0]["id"], "cut");
    assert_eq!(calls[0]["extra"], "keep");
}
#[tokio::test]
async fn truncated_stream_and_legacy_null_history_replay_on_both_anthropic_transports() {
    for oauth in [false, true] {
        for streamed in [false, true] {
            let server = MockServer::start().await;
            let mut client = AnthropicClient::new(AnthropicConfig {
                base_url: server.uri(),
                ..Default::default()
            })
            .unwrap();
            if oauth {
                client = client.with_claude_oauth().unwrap();
            }
            let auth = ProviderAuth::new("test");
            let mut input = request();
            if streamed {
                let name = if oauth { "_read" } else { "read" };
                let frames = [
                    ("message_start", json!({"type":"message_start","message":{"id":"cut","type":"message","role":"assistant","model":"claude-test","content":[],"usage":{"input_tokens":10,"output_tokens":0}}})),
                    ("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"cut","name":name,"input":{},"extra":"keep"}})),
                    ("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}})),
                    ("content_block_stop", json!({"type":"content_block_stop","index":0})),
                    ("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":1}})),
                    ("message_stop", json!({"type":"message_stop"})),
                ].into_iter().map(|(event,data)| format!("event: {event}\ndata: {data}\n\n")).collect::<String>();
                Mock::given(method("POST"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .insert_header("content-type", "text/event-stream")
                            .set_body_string(frames),
                    )
                    .up_to_n_times(1)
                    .mount(&server)
                    .await;
                let mut stream = client.stream(&auth, &input).await.unwrap();
                let mut completed = None;
                while let Some(event) = stream.next().await {
                    if let StreamEvent::Completed { response } = event.unwrap() {
                        completed = Some(response);
                    }
                }
                let response = completed.unwrap();
                assert_eq!(response.finish_reason, FinishReason::Length);
                assert!(
                    response
                        .message
                        .content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::ToolUse {input,..} if input.is_null()))
                );
                input.messages.push(response.message);
                input.messages.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_call_id: "cut".into(),
                        content: vec![ToolResultContent::Text {
                            text: "Malformed arguments; tool was not run.".into(),
                        }],
                        is_error: true,
                    }],
                });
            } else {
                input.messages.extend(legacy());
            }
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_json(response()))
                .mount(&server)
                .await;
            let before = input.messages.clone();
            let signed = client.send(&auth, &input).await.unwrap();
            assert_eq!(
                input.messages, before,
                "outbound repair must not mutate stored history"
            );
            input.messages.push(signed.message);
            input.messages.push(Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "next".into(),
                }],
            });
            client.send(&auth, &input).await.unwrap();
            let captured = server.received_requests().await.unwrap();
            let repair_index = usize::from(streamed);
            assert_eq!(captured.len(), repair_index + 2, "no automatic retry");
            assert_repaired(&body(&captured[repair_index]));
            let last = body(captured.last().unwrap());
            assert_repaired(&last);
            assert!(
                last["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|m| m["content"].as_array().unwrap())
                    .any(|b| b["type"] == "thinking" && b["signature"] == "sig"),
                "normalization must agree with the prefix used to bind signed thinking"
            );
        }
    }
}
