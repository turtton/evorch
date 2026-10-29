//! `/v1/responses` serves Responses SSE whose cached tokens come from the input prefix.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use providers::provider::openai::{OpenAiClient, OpenAiConfig};
use providers::{
    ChatRequest, ContentBlock, FinishReason, Message, ProviderAuth, ProviderClient, Role,
};
use serde_json::{Value, json};

/// Posts one Responses request and returns its SSE `data` payloads in order.
fn post_responses(server: &StreamingMockOpenAi, body: &Value) -> Vec<Value> {
    let address = server
        .base_url()
        .strip_prefix("http://")
        .unwrap()
        .strip_suffix("/v1")
        .unwrap()
        .to_owned();
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let body = body.to_string();
    write!(stream, "POST /v1/responses HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer k\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    assert!(headers.starts_with("HTTP/1.1 200 OK"), "{headers}");
    assert!(
        headers.contains("Content-Type: text/event-stream"),
        "{headers}"
    );
    body.split("\n\n")
        .filter_map(|frame| frame.lines().find_map(|line| line.strip_prefix("data: ")))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}

/// Returns `(input_tokens, cached_tokens)` from the terminal `response.completed` event.
fn usage(events: &[Value]) -> (u64, u64) {
    let completed = events.last().unwrap();
    assert_eq!(completed["type"], "response.completed");
    let usage = &completed["response"]["usage"];
    let input = usage["input_tokens"].as_u64().unwrap();
    let cached = usage["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap();
    assert!(cached <= input);
    assert_eq!(usage["output_tokens"], 7);
    assert_eq!(usage["total_tokens"], input + 7);
    (input, cached)
}

fn scripts(count: usize) -> Vec<ScriptedResponse> {
    (0..count)
        .map(|index| {
            ScriptedResponse::text_stream(&format!("resp-{index}"), "m", ["ok"]).with_usage(999, 7)
        })
        .collect()
}

fn message(role: &str, text: &str) -> Value {
    let kind = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    json!({"type": "message", "role": role, "content": [{"type": kind, "text": text}]})
}

// Given: a prompt-cache mock and a Responses request with instructions, tools, and tool history
// When: the input is appended, an earlier item is rewritten, then the new input is extended
// Then: response.completed reports the reused input prefix, rewriting loses it, and the rewritten
// input warms a new prefix
#[test]
fn responses_prompt_cache_follows_the_input_prefix() {
    let server = StreamingMockOpenAi::spawn_with_prompt_cache(scripts(4));
    let mut request = json!({
        "model": "m", "stream": true, "store": false,
        "instructions": "Keep the existing transcript.",
        "tools": [{"type": "function", "name": "read", "description": "Read",
            "parameters": {"type": "object"}, "strict": false}],
        "prompt_cache_key": "run-1",
        "input": [
            message("user", "Read the tool result."),
            {"type": "function_call", "call_id": "call-1", "name": "read", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call-1", "output": "large output ".repeat(200)},
        ],
    });
    let (first_input, first_cached) = usage(&post_responses(&server, &request));
    assert_eq!(first_cached, 0);

    let previous = request.clone();
    request["input"]
        .as_array_mut()
        .unwrap()
        .extend([message("assistant", "Read."), message("user", "Continue.")]);
    assert_append_only(CacheProtocol::Codex, &previous, &request).unwrap();
    let (appended_input, appended_cached) = usage(&post_responses(&server, &request));
    assert!(appended_input > first_input);
    assert_eq!(appended_cached, first_input);

    request["input"][2]["output"] = json!("Full output: /tmp/output.txt");
    let (rewritten_input, rewritten_cached) = usage(&post_responses(&server, &request));
    assert!(rewritten_cached < first_input);
    assert!(rewritten_cached < rewritten_input);

    request["input"]
        .as_array_mut()
        .unwrap()
        .push(message("assistant", "Continued."));
    assert_eq!(usage(&post_responses(&server, &request)).1, rewritten_input);
    assert_eq!(server.remaining_scripts(), 0);
}

// Given: two Responses requests with the same input but different instructions or tools
// When: both are sent to the prompt-cache mock
// Then: changing either setting invalidates the reusable prefix
#[test]
fn responses_prompt_cache_includes_instructions_and_tools() {
    for changed in ["instructions", "tools"] {
        let server = StreamingMockOpenAi::spawn_with_prompt_cache(scripts(2));
        let mut request = json!({
            "model": "m", "stream": true, "instructions": "Stable.",
            "tools": [{"type": "function", "name": "read", "description": "Read",
                "parameters": {"type": "object"}}],
            "input": [message("user", "hello")],
        });
        let (input, _) = usage(&post_responses(&server, &request));
        match changed {
            "instructions" => request["instructions"] = json!("Changed."),
            "tools" => request["tools"][0]["description"] = json!("Changed tool"),
            _ => unreachable!(),
        }
        assert!(
            usage(&post_responses(&server, &request)).1 < input,
            "{changed}"
        );
    }
}

fn chat_request(messages: Vec<Message>) -> ChatRequest {
    ChatRequest {
        responses_replay: None,
        model: "m".into(),
        messages,
        tools: vec![providers::ToolSpec {
            name: "read".into(),
            description: "Read".into(),
            input_schema: json!({"type": "object"}),
        }],
        temperature: None,
        max_tokens: None,
        reasoning_effort: None,
        service_tier: None,
        output_schema: None,
        observation: Some(providers::ObservationContext {
            attempts: None,
            turn: None,
            run_id: "run-1".into(),
            purpose: Default::default(),
        }),
    }
}

// Given: the real OpenAI Responses client pointed at the prompt-cache mock
// When: a tool call round trip is followed by a text answer
// Then: the client decodes the scripted function call and text, and the second request reports
// the first request's whole input as cached
#[tokio::test(flavor = "multi_thread")]
async fn openai_client_round_trips_tool_calls_with_input_derived_cache() {
    let server = StreamingMockOpenAi::spawn_with_prompt_cache(vec![
        ScriptedResponse::tool_call("resp-1", "m", 0, "call-1", "read", ["{\"pa", "th\":\"a\"}"]),
        ScriptedResponse::text_stream("resp-2", "m", ["Done", "."]),
    ]);
    let client = OpenAiClient::new(OpenAiConfig {
        base_url: server.base_url(),
        ..OpenAiConfig::default()
    })
    .unwrap();
    let auth = ProviderAuth::new("k");
    let mut messages = vec![
        Message {
            role: Role::System,
            content: vec![ContentBlock::Text {
                text: "Stable instructions".into(),
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Read a".into(),
            }],
        },
    ];

    let first = client
        .send(&auth, &chat_request(messages.clone()))
        .await
        .unwrap();
    assert_eq!(first.finish_reason, FinishReason::ToolUse);
    assert_eq!(
        first.message.content,
        vec![ContentBlock::ToolUse {
            id: "call-1".into(),
            name: "read".into(),
            input: json!({"path": "a"}),
        }]
    );
    assert_eq!(first.usage.cache_read_tokens, 0);
    messages.push(first.message);
    messages.push(Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_call_id: "call-1".into(),
            content: vec![providers::ToolResultContent::Text {
                text: "contents".into(),
            }],
            is_error: false,
        }],
    });
    let second = client.send(&auth, &chat_request(messages)).await.unwrap();

    assert_eq!(second.finish_reason, FinishReason::Stop);
    assert_eq!(
        second.message.content,
        vec![ContentBlock::Text {
            text: "Done.".into()
        }]
    );
    assert_eq!(second.usage.cache_read_tokens, first.usage.input_tokens);
    let requests = server.recorded_requests();
    assert!(
        requests
            .iter()
            .all(|request| request.path == "/v1/responses")
    );
    assert_append_only(CacheProtocol::Codex, &requests[0].body, &requests[1].body).unwrap();
}
