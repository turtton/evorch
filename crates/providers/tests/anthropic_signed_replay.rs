//! Signed thinking must survive streaming, serialization, and tool-result replay.
use futures_util::StreamExt;
use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
use providers::provider::anthropic::{AnthropicClient, AnthropicConfig};
use providers::{ChatRequest, Message, ProviderAuth, ProviderClient, StreamEvent};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn input() -> ChatRequest {
    serde_json::from_value(json!({
        "model":"claude-fable-5-1",
        "messages":[
            {"role":"system","content":[{"type":"text","text":"Stable instructions"}]},
            {"role":"user","content":[{"type":"text","text":"Read the report"}]}
        ],
        "tools":[{"name":"read","description":"Read report","input_schema":{"type":"object"}}]
    }))
    .unwrap()
}

fn blocks(oauth: bool) -> Vec<Value> {
    vec![
        json!({"type":"thinking","thinking":"","signature":"opaque-signature-1"}),
        json!({"type":"text","text":"Checking. ","citations":[{"type":"char_location","document_index":0,"document_title":"Report","start_char_index":0,"end_char_index":8,"cited_text":"Evidence"}]}),
        json!({"type":"server_tool_use","id":"srvtoolu-1","name":"web_search","input":{"query":"report"}}),
        json!({"type":"web_search_tool_result","tool_use_id":"srvtoolu-1","content":[]}),
        json!({"type":"redacted_thinking","data":"opaque-redacted-data"}),
        json!({"type":"thinking","thinking":"Use the local tool.","signature":"opaque-signature-2"}),
        json!({"type":"tool_use","id":"toolu-1","name":if oauth {"_read"} else {"read"},"input":{"path":"report"}}),
    ]
}

fn sse(blocks: &[Value]) -> String {
    let mut events = vec![
        json!({"type":"message_start","message":{"role":"assistant","content":[],"usage":{"input_tokens":10,"output_tokens":0}}}),
    ];
    for (index, block) in blocks.iter().enumerate() {
        let mut start = block.clone();
        let delta = match block["type"].as_str().unwrap() {
            "thinking" => {
                start["thinking"] = json!("");
                start["signature"] = json!("");
                vec![
                    json!({"type":"thinking_delta","thinking":block["thinking"]}),
                    json!({"type":"signature_delta","signature":block["signature"]}),
                ]
            }
            "text" => {
                start["text"] = json!("");
                start["citations"] = json!([]);
                vec![
                    json!({"type":"text_delta","text":block["text"]}),
                    json!({"type":"citations_delta","citation":block["citations"][0]}),
                ]
            }
            "tool_use" | "server_tool_use" => {
                start["input"] = json!({});
                vec![json!({"type":"input_json_delta","partial_json":block["input"].to_string()})]
            }
            _ => vec![],
        };
        events.push(json!({"type":"content_block_start","index":index,"content_block":start}));
        for delta in delta {
            events.push(json!({"type":"content_block_delta","index":index,"delta":delta}));
        }
        events.push(json!({"type":"content_block_stop","index":index}));
    }
    events.push(json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":20}}));
    events.push(json!({"type":"message_stop"}));
    events
        .into_iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect()
}

async fn exercise(streaming: bool, oauth: bool) {
    let server = MockServer::start().await;
    let expected = blocks(oauth);
    let response = if streaming {
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(sse(&expected))
    } else {
        ResponseTemplate::new(200).set_body_json(json!({"role":"assistant","content":expected,"stop_reason":"tool_use","usage":{"input_tokens":10,"output_tokens":20}}))
    };
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"stream":streaming})))
        .respond_with(response)
        .mount(&server)
        .await;
    if streaming {
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"stream":false})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"role":"assistant","content":expected,"stop_reason":"tool_use","usage":{"input_tokens":10,"output_tokens":20}})))
            .mount(&server)
            .await;
    }
    let mut client = AnthropicClient::new(AnthropicConfig {
        base_url: server.uri(),
        ..Default::default()
    })
    .unwrap();
    if oauth {
        client = client.with_claude_oauth();
    }
    let auth = ProviderAuth::new("fixture-token");
    let mut request = input();
    let response = if streaming {
        let events = client
            .stream(&auth, &request)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        let events = events
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("valid Anthropic SSE must complete");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::ToolCallDelta { name: Some(_), .. }))
                .count(),
            1,
            "server-side tools must not become local calls"
        );
        match events.into_iter().last().unwrap() {
            StreamEvent::Completed { response } => response,
            event => panic!("missing completion: {event:?}"),
        }
    } else {
        client.send(&auth, &request).await.unwrap()
    };
    // Reconstruct canonical history as a persisted snapshot would.
    let restored: Message =
        serde_json::from_str(&serde_json::to_string(&response.message).unwrap()).unwrap();
    request.messages.push(restored);
    request.messages.push(serde_json::from_value(json!({"role":"user","content":[{"type":"tool_result","tool_call_id":"toolu-1","content":[{"type":"text","text":"report contents"}],"is_error":false}]})).unwrap());
    client.send(&auth, &request).await.unwrap();
    request.messages.push(
        serde_json::from_value(
            json!({"role":"user","content":[{"type":"text","text":"Continue"}]}),
        )
        .unwrap(),
    );
    client.send(&auth, &request).await.unwrap();
    let captured = server.received_requests().await.unwrap();
    let bodies: Vec<Value> = captured
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    for body in bodies.iter().skip(1) {
        assert_eq!(
            body["messages"][1]["content"],
            json!(expected),
            "the original block order, signatures, citations, and server results must be replayed verbatim"
        );
    }
    for pair in bodies.windows(2) {
        assert_append_only(CacheProtocol::Anthropic, &pair[0], &pair[1]).unwrap();
    }
}

#[tokio::test]
async fn signed_thinking_streams_and_replays_after_serialization() {
    for oauth in [false, true] {
        exercise(true, oauth).await;
    }
}

#[tokio::test]
async fn signed_thinking_nonstream_response_replays_after_serialization() {
    for oauth in [false, true] {
        exercise(false, oauth).await;
    }
}

#[tokio::test]
async fn replaced_prefix_omits_old_signatures_and_reuses_new_ones() {
    for oauth in [false, true] {
        let server = MockServer::start().await;
        let signed = json!([
            {"type":"thinking","thinking":"","signature":"opaque-bound-signature"},
            {"type":"redacted_thinking","data":"opaque-bound-redacted"},
            {"type":"text","text":"Answer"}
        ]);
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "role":"assistant","content":signed,"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":20}
        }))).expect(3).mount(&server).await;
        let mut client = AnthropicClient::new(AnthropicConfig {
            base_url: server.uri(),
            ..Default::default()
        })
        .unwrap();
        if oauth {
            client = client.with_claude_oauth();
        }
        let auth = ProviderAuth::new("fixture-token");
        let mut request = input();
        let old = client.send(&auth, &request).await.unwrap();
        request.messages.push(old.message);
        request.messages.push(
            serde_json::from_value(
                json!({"role":"user","content":[{"type":"text","text":"Continue"}]}),
            )
            .unwrap(),
        );
        // A compaction or changed system/tools/history establishes a new prefix.
        request.messages[1].content[0] =
            serde_json::from_value(json!({"type":"text","text":"Replacement conversation"}))
                .unwrap();
        let fresh = client.send(&auth, &request).await.unwrap();
        request.messages.push(fresh.message);
        request.messages.push(
            serde_json::from_value(
                json!({"role":"user","content":[{"type":"text","text":"Continue again"}]}),
            )
            .unwrap(),
        );
        client.send(&auth, &request).await.unwrap();
        let captured = server.received_requests().await.unwrap();
        let bodies: Vec<Value> = captured
            .iter()
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect();
        for body in bodies.iter().skip(1) {
            let old = body["messages"][1]["content"].as_array().unwrap();
            assert!(
                old.iter().all(
                    |block| block["type"] != "thinking" && block["type"] != "redacted_thinking"
                )
            );
            assert!(old.iter().any(|block| block["text"] == "Answer"));
        }
        assert_eq!(bodies[2]["messages"][3]["content"][0], signed[0]);
        assert_eq!(bodies[2]["messages"][3]["content"][1], signed[1]);
        assert_append_only(CacheProtocol::Anthropic, &bodies[1], &bodies[2]).unwrap();
    }
}

#[tokio::test]
async fn tool_schema_changes_invalidate_bound_thinking() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "role":"assistant","content":[
                {"type":"thinking","thinking":"","signature":"schema-bound-signature"},
                {"type":"text","text":"Answer"}
            ],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":2}
        })))
        .expect(2)
        .mount(&server)
        .await;
    let client = AnthropicClient::new(AnthropicConfig {
        base_url: server.uri(),
        ..Default::default()
    })
    .unwrap();
    let auth = ProviderAuth::new("fixture-token");
    let mut request = input();
    request.tools[0].input_schema =
        json!({"type":"object","properties":{"cache_control":{"type":"string"}}});
    let response = client.send(&auth, &request).await.unwrap();
    request.messages.push(response.message);
    request.messages.push(
        serde_json::from_value(
            json!({"role":"user","content":[{"type":"text","text":"Continue"}]}),
        )
        .unwrap(),
    );
    // This is schema data, not an Anthropic breakpoint annotation.
    request.tools[0].input_schema["properties"]["cache_control"]["type"] = json!("integer");
    client.send(&auth, &request).await.unwrap();
    let captured = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&captured[1].body).unwrap();
    assert!(
        body["messages"][1]["content"]
            .as_array()
            .unwrap()
            .iter()
            .all(|block| block["type"] != "thinking")
    );
}
