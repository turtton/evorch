//! Responses `reasoning` output items: the mock streams a provisional `added` item and a
//! different confirmed `done` item, and the real Responses client replays only the done
//! item verbatim, in output order, inside the input-derived cached prefix.

use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
use mock_openai::{ReasoningItem, ScriptedResponse, StreamingMockOpenAi};
use providers::provider::openai::{OpenAiClient, OpenAiConfig};
use providers::{
    ChatRequest, ContentBlock, FinishReason, Message, ProviderAuth, ProviderClient,
    ResponsesReplayContext, ResponsesSpeed, Role, ToolResultContent,
};
use serde_json::{Value, json};

/// Decodes rendered SSE frames, checking each `event:` line names its payload type.
fn decode(frames: &[String]) -> Vec<Value> {
    frames
        .iter()
        .map(|frame| {
            let (event, data) = frame
                .strip_suffix("\n\n")
                .and_then(|frame| frame.split_once('\n'))
                .unwrap_or_else(|| panic!("malformed frame {frame:?}"));
            let data: Value = serde_json::from_str(data.strip_prefix("data: ").unwrap()).unwrap();
            assert_eq!(
                Some(event),
                data["type"]
                    .as_str()
                    .map(|t| format!("event: {t}"))
                    .as_deref()
            );
            data
        })
        .collect()
}

fn types_at(events: &[Value], index: usize) -> Vec<&str> {
    events
        .iter()
        .filter(|event| event["output_index"] == index)
        .map(|event| event["type"].as_str().unwrap())
        .collect()
}

fn item_event<'a>(events: &'a [Value], kind: &str, index: usize) -> &'a Value {
    let matching: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == kind && event["output_index"] == index)
        .collect();
    assert_eq!(matching.len(), 1, "{kind} at {index}: {events:?}");
    &matching[0]["item"]
}

// Given: a tool call at output index 1 preceded by a reasoning item at index 0
// When: the script is rendered as Responses SSE
// Then: the reasoning item streams a provisional added item, its summary part and
// deltas, then a done item with a different, final ciphertext and the full summary
#[test]
fn reasoning_streams_a_provisional_added_item_then_the_final_done_item() {
    let reasoning = ReasoningItem::new(0, "rs_1", "final-R1")
        .with_added_encrypted_content("partial-R1")
        .with_summary(["Plan ", "the read."]);
    let script = ScriptedResponse::tool_call("resp-1", "m", 1, "call-1", "read", ["{}"])
        .with_reasoning(reasoning.clone());
    let events = decode(&script.responses_frames());

    assert_eq!(
        types_at(&events, 0),
        [
            "response.output_item.added",
            "response.reasoning_summary_part.added",
            "response.reasoning_summary_text.delta",
            "response.reasoning_summary_text.delta",
            "response.reasoning_summary_text.done",
            "response.reasoning_summary_part.done",
            "response.output_item.done",
        ]
    );
    assert_eq!(
        item_event(&events, "response.output_item.added", 0),
        &json!({"type": "reasoning", "id": "rs_1", "summary": [],
            "encrypted_content": "partial-R1"})
    );
    let done = json!({"type": "reasoning", "id": "rs_1",
        "summary": [{"type": "summary_text", "text": "Plan the read."}],
        "encrypted_content": "final-R1"});
    assert_eq!(item_event(&events, "response.output_item.done", 0), &done);
    assert_eq!(reasoning.done_item(), done);
    let deltas: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "response.reasoning_summary_text.delta")
        .map(|event| {
            assert_eq!(
                (&event["item_id"], &event["summary_index"]),
                (&json!("rs_1"), &json!(0))
            );
            event["delta"].as_str().unwrap()
        })
        .collect();
    assert_eq!(deltas, ["Plan ", "the read."]);
    assert_eq!(
        types_at(&events, 1),
        [
            "response.output_item.added",
            "response.function_call_arguments.delta",
            "response.output_item.done",
        ]
    );
}

// Given: reasoning items before and after a tool call and around text
// When: the scripts are rendered as Responses SSE
// Then: every output item streams in output-index order before response.completed,
// an omitted provisional ciphertext stays absent, and extra fields reach only done
#[test]
fn output_items_stream_in_output_index_order() {
    let tool = ScriptedResponse::tool_call("resp-1", "m", 1, "call-1", "read", ["{}"])
        .with_reasoning(ReasoningItem::new(2, "rs_after", "after"))
        .with_reasoning(
            ReasoningItem::new(0, "rs_before", "before")
                .without_added_encrypted_content()
                .with_extra("status", json!("completed"))
                .with_extra("future_field", json!({"kept": true})),
        );
    let events = decode(&tool.responses_frames());
    let order: Vec<_> = events
        .iter()
        .filter_map(|event| event["output_index"].as_u64())
        .collect();
    let mut sorted = order.clone();
    sorted.sort_unstable();
    assert_eq!(order, sorted, "{events:?}");
    assert_eq!(
        item_event(&events, "response.output_item.added", 0),
        &json!({"type": "reasoning", "id": "rs_before", "summary": []})
    );
    assert_eq!(
        item_event(&events, "response.output_item.done", 0),
        &json!({"type": "reasoning", "id": "rs_before", "summary": [],
            "encrypted_content": "before", "status": "completed",
            "future_field": {"kept": true}})
    );
    assert_eq!(
        item_event(&events, "response.output_item.done", 2)["encrypted_content"],
        "after"
    );
    assert_eq!(events.last().unwrap()["type"], "response.completed");

    let text = ScriptedResponse::text_stream("resp-2", "m", ["Do", "ne."])
        .with_text_output_index(1)
        .with_reasoning(ReasoningItem::new(0, "rs_text", "text-R"));
    let events = decode(&text.responses_frames());
    assert_eq!(
        types_at(&events, 1),
        ["response.output_text.delta", "response.output_text.delta"]
    );
    let first_text = events
        .iter()
        .position(|event| event["type"] == "response.output_text.delta")
        .unwrap();
    let reasoning_done = events
        .iter()
        .position(|event| event["type"] == "response.output_item.done")
        .unwrap();
    assert!(reasoning_done < first_text, "{events:?}");
    assert!(
        events
            .iter()
            .filter(|event| event["type"] == "response.output_text.delta")
            .all(|event| event["item_id"] == "msg_1")
    );
}

// Given: scripts with reasoning items
// When: they are rendered for Chat Completions (SSE and JSON)
// Then: the output equals the same script without reasoning, which Chat cannot carry
#[test]
fn chat_completions_renderings_carry_no_reasoning_item() {
    let plain = ScriptedResponse::tool_call("resp-1", "m", 1, "call-1", "read", ["{}"]);
    let reasoning = plain
        .clone()
        .with_reasoning(ReasoningItem::new(0, "rs_1", "secret-R1").with_summary(["s"]));
    assert_eq!(reasoning.frames(), plain.frames());
    assert_eq!(
        reasoning.to_non_streaming_json(),
        plain.to_non_streaming_json()
    );
    assert!(!reasoning.body().contains("secret-R1"));
}

fn request(messages: Vec<Message>, replay: Option<ResponsesReplayContext>) -> ChatRequest {
    ChatRequest {
        responses_replay: replay,
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

fn text(role: Role, text: &str) -> Message {
    Message {
        role,
        content: vec![ContentBlock::Text { text: text.into() }],
    }
}

fn input(body: &Value) -> &[Value] {
    body["input"].as_array().unwrap()
}

// Given: the real OpenAI Responses client and the input-derived prompt-cache mock, whose
// first reply is reasoning R1 (provisional ciphertext differs) followed by a tool call
// When: the tool result, a reply with reasoning R2, and a follow-up are sent
// Then: the canonical message keeps the done R1 before the call, each later request
// replays R1/R2 verbatim in output order (never the provisional value), requests stay
// append-only with the include fixed, the mock reports the previous input as cached,
// and altering the replayed ciphertext loses that prefix
#[tokio::test(flavor = "multi_thread")]
async fn openai_client_replays_done_reasoning_inside_the_cached_prefix() {
    let r1 = ReasoningItem::new(0, "rs_1", "final-R1")
        .with_added_encrypted_content("partial-R1")
        .with_summary(["Plan the read."]);
    let r2 = ReasoningItem::new(0, "rs_2", "final-R2").with_added_encrypted_content("partial-R2");
    let server = StreamingMockOpenAi::spawn_with_prompt_cache(vec![
        ScriptedResponse::tool_call("resp-1", "m", 1, "call-1", "read", ["{\"path\":\"a\"}"])
            .with_reasoning(r1.clone()),
        ScriptedResponse::text_stream("resp-2", "m", ["Done."])
            .with_text_output_index(1)
            .with_reasoning(r2.clone()),
        ScriptedResponse::text_stream("resp-3", "m", ["Again."]),
        ScriptedResponse::text_stream("resp-4", "m", ["Mutated."]),
    ]);
    let client = OpenAiClient::new(OpenAiConfig {
        base_url: server.base_url(),
        ..OpenAiConfig::default()
    })
    .unwrap();
    let auth = ProviderAuth::new("k");
    let mut messages = vec![
        text(Role::System, "Stable instructions"),
        text(Role::User, "Read a"),
    ];
    let mut context = ResponsesReplayContext {
        route_model: "m".into(),
        speed: ResponsesSpeed::Standard,
        expected_scope: None,
    };
    let scope = client
        .resolve_responses_scope(&auth, &request(messages.clone(), Some(context.clone())))
        .await
        .unwrap()
        .expect("Responses clients resolve a replay scope");
    context.expected_scope = Some(scope.clone());

    let first = client
        .send(&auth, &request(messages.clone(), Some(context.clone())))
        .await
        .unwrap();
    assert_eq!(first.finish_reason, FinishReason::ToolUse);
    let replayable: Vec<_> = first
        .message
        .content
        .iter()
        .filter(|block| !matches!(block, ContentBlock::Reasoning { .. }))
        .cloned()
        .collect();
    assert_eq!(
        replayable,
        [
            ContentBlock::ResponsesReasoning {
                item: r1.done_item(),
                origin: scope.clone(),
            },
            ContentBlock::ToolUse {
                id: "call-1".into(),
                name: "read".into(),
                input: json!({"path": "a"}),
            },
        ]
    );
    messages.push(first.message);
    messages.push(Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_call_id: "call-1".into(),
            content: vec![ToolResultContent::Text {
                text: "contents".into(),
            }],
            is_error: false,
        }],
    });
    let second = client
        .send(&auth, &request(messages.clone(), Some(context.clone())))
        .await
        .unwrap();
    assert_eq!(second.usage.cache_read_tokens, first.usage.input_tokens);
    messages.push(second.message);
    messages.push(text(Role::User, "Again"));
    let third = client
        .send(&auth, &request(messages.clone(), Some(context.clone())))
        .await
        .unwrap();
    assert_eq!(third.usage.cache_read_tokens, second.usage.input_tokens);

    let bodies: Vec<_> = server
        .recorded_requests()
        .into_iter()
        .map(|request| request.body)
        .collect();
    for body in &bodies {
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert!(!body.to_string().contains("partial-R"), "{body}");
    }
    let r1_at = input(&bodies[1])
        .iter()
        .position(|item| item == &r1.done_item())
        .unwrap_or_else(|| panic!("R1 is not replayed: {}", bodies[1]));
    assert_eq!(input(&bodies[1])[r1_at + 1]["type"], "function_call");
    assert_eq!(input(&bodies[1])[r1_at + 1]["call_id"], "call-1");
    assert_eq!(input(&bodies[1])[r1_at + 2]["type"], "function_call_output");
    assert_eq!(input(&bodies[2])[r1_at], r1.done_item());
    let r2_at = input(&bodies[2])
        .iter()
        .position(|item| item == &r2.done_item())
        .unwrap_or_else(|| panic!("R2 is not replayed: {}", bodies[2]));
    assert!(r2_at > r1_at + 2);
    assert_eq!(input(&bodies[2])[r2_at + 1]["role"], "assistant");
    assert_append_only(CacheProtocol::Codex, &bodies[0], &bodies[1]).unwrap();
    assert_append_only(CacheProtocol::Codex, &bodies[1], &bodies[2]).unwrap();

    // Negative: a rewritten ciphertext is a different prefix for both observations.
    for message in &mut messages {
        for block in &mut message.content {
            if let ContentBlock::ResponsesReasoning { item, .. } = block
                && item["id"] == "rs_1"
            {
                item["encrypted_content"] = json!("tampered-R1");
            }
        }
    }
    let tampered = client
        .send(&auth, &request(messages, Some(context)))
        .await
        .unwrap();
    assert!(tampered.usage.cache_read_tokens < third.usage.input_tokens);
    let tampered_body = server.recorded_requests().pop().unwrap().body;
    assert_eq!(
        assert_append_only(CacheProtocol::Codex, &bodies[2], &tampered_body),
        Err(format!("previous conversation item {r1_at} changed"))
    );
    assert_eq!(server.remaining_scripts(), 0);
}
