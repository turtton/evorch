use std::time::Duration;

use futures_util::StreamExt;
use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use providers::provider::openai::{OpenAiClient, OpenAiConfig};
use providers::provider::openai_compatible::OpenAiCompatibleClient;
use providers::{
    ChatRequest, ContentBlock, FinishReason, Message, ProviderAuth, ProviderClient, Role,
    StreamEvent, ToolResultContent,
};
use serde_json::{Value, json};

#[tokio::test(flavor = "multi_thread")]
async fn custom_scenario_round_trips_responses_and_chat_with_recorded_history() {
    let fragments = [
        "*** Begin Patch\n",
        "*** Add File: 日本語.txt\n+\"\\\t\n",
        "*** End Patch",
    ];
    for responses in [true, false] {
        let server = StreamingMockOpenAi::spawn_with_prompt_cache(vec![
            ScriptedResponse::custom_tool_call(
                "resp-1",
                "m",
                0,
                "call-1",
                "apply_patch",
                fragments,
            ),
            ScriptedResponse::text_stream("resp-2", "m", ["Done."]),
        ]);
        let client: Box<dyn ProviderClient> = if responses {
            Box::new(
                OpenAiClient::new(OpenAiConfig {
                    base_url: server.base_url(),
                    ..OpenAiConfig::default()
                })
                .unwrap(),
            )
        } else {
            Box::new(
                OpenAiCompatibleClient::new(
                    server.base_url(),
                    "mock",
                    Duration::from_secs(10),
                    None,
                )
                .unwrap(),
            )
        };
        let auth = ProviderAuth::new("k");
        let mut request: ChatRequest = serde_json::from_value(json!({
            "model": "m", "messages": [
                {"role": "system", "content": [{"type": "text", "text": "stable"}]},
                {"role": "user", "content": [{"type": "text", "text": "patch"}]}
            ], "tools": [{"name": "apply_patch", "description": "patch", "input_schema": apply_patch::tool_contract::json_schema()}]
        })).unwrap();
        let first = client.send(&auth, &request).await.unwrap();
        assert_eq!(first.finish_reason, FinishReason::ToolUse);
        assert_eq!(
            first.message.content,
            vec![ContentBlock::ToolUse {
                id: "call-1".into(),
                name: "apply_patch".into(),
                input: json!({"input": fragments.concat()})
            }]
        );
        request.messages.push(first.message);
        request.messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: "call-1".into(),
                content: vec![ToolResultContent::Text { text: "ok".into() }],
                is_error: false,
            }],
        });
        let second = client.send(&auth, &request).await.unwrap();
        assert_eq!(second.finish_reason, FinishReason::Stop);
        assert_eq!(second.usage.cache_read_tokens, first.usage.input_tokens);
        let recorded = server.recorded_requests();
        assert_eq!(recorded.len(), 2);
        let protocol = if responses {
            CacheProtocol::Codex
        } else {
            CacheProtocol::OpenAi
        };
        assert_append_only(protocol, &recorded[0].body, &recorded[1].body).unwrap();
        if responses {
            assert_eq!(recorded[0].body["tools"][0]["type"], "custom");
            assert_eq!(recorded[1].body["input"][1]["type"], "custom_tool_call");
            assert_eq!(
                recorded[1].body["input"][2]["type"],
                "custom_tool_call_output"
            );
        } else {
            assert_eq!(recorded[0].body["tools"][0]["type"], "function");
            assert_eq!(
                recorded[1].body["messages"][2]["tool_calls"][0]["type"],
                "function"
            );
            assert_eq!(recorded[1].body["messages"][3]["role"], "tool");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn custom_scenario_chat_stream_preserves_raw_input_as_function_arguments() {
    for (fragments, input) in [
        (vec![""], ""),
        (vec!["\"\\", "\n\t\0", "日本語"], "\"\\\n\t\0日本語"),
    ] {
        let server = StreamingMockOpenAi::spawn(vec![ScriptedResponse::custom_tool_call(
            "resp",
            "m",
            0,
            "call",
            "apply_patch",
            fragments,
        )]);
        let client =
            OpenAiCompatibleClient::new(server.base_url(), "mock", Duration::from_secs(10), None)
                .unwrap();
        let request: ChatRequest = serde_json::from_value(json!({
            "model": "m", "messages": [{"role": "user", "content": [{"type": "text", "text": "patch"}]}]
        })).unwrap();
        let events = client
            .stream(&ProviderAuth::new("k"), &request)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let Some(StreamEvent::Completed { response }) = events.last() else {
            panic!("stream did not complete: {events:?}");
        };
        assert_eq!(response.finish_reason, FinishReason::ToolUse);
        assert_eq!(
            response.message.content,
            vec![ContentBlock::ToolUse {
                id: "call".into(),
                name: "apply_patch".into(),
                input: json!({"input": input})
            }]
        );
        assert_eq!(server.recorded_requests()[0].path, "/v1/chat/completions");
        assert!(server.recorded_requests()[0].stream);
    }
}

#[test]
fn custom_scenario_frames_match_responses_event_order_and_chat_arguments() {
    let scenario = ScriptedResponse::custom_tool_call(
        "resp",
        "m",
        3,
        "call",
        "apply_patch",
        ["a\n", "日本語\\\""],
    );
    let events: Vec<Value> = scenario
        .responses_frames()
        .iter()
        .map(|frame| {
            serde_json::from_str(
                frame
                    .lines()
                    .find_map(|line| line.strip_prefix("data: "))
                    .unwrap(),
            )
            .unwrap()
        })
        .collect();
    assert_eq!(
        events
            .iter()
            .map(|e| e["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![
            "response.created",
            "response.output_item.added",
            "response.custom_tool_call_input.delta",
            "response.custom_tool_call_input.delta",
            "response.custom_tool_call_input.done",
            "response.output_item.done",
            "response.completed"
        ]
    );
    assert_eq!(events[1]["item"]["input"], "");
    assert_eq!(events[1]["item"]["type"], "custom_tool_call");
    for event in &events[2..=4] {
        assert_eq!(event["item_id"], "ctc_call");
        assert_eq!(event["output_index"], 3);
        assert!(event.get("call_id").is_none());
    }
    assert_eq!(events[4]["input"], "a\n日本語\\\"");
    assert_eq!(events[5]["item"]["input"], events[4]["input"]);
    let non_streaming = scenario.to_non_streaming_json();
    assert_eq!(
        serde_json::from_str::<Value>(
            non_streaming["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap()
        )
        .unwrap(),
        json!({"input": "a\n日本語\\\""})
    );
}

#[test]
fn cache_contract_rejects_empty_custom_identifiers() {
    let request = json!({"model": "m", "instructions": "", "input": [
        {"type": "custom_tool_call", "call_id": "call", "name": "apply_patch", "input": ""},
        {"type": "custom_tool_call_output", "call_id": "call", "output": ""}
    ]});
    for (index, field) in [(0, "call_id"), (0, "name"), (1, "call_id")] {
        let mut malformed = request.clone();
        malformed["input"][index][field] = json!("");
        assert!(
            assert_append_only(CacheProtocol::Codex, &malformed, &malformed).is_err(),
            "input[{index}].{field} must not be empty"
        );
    }
}

#[test]
fn cache_contract_accepts_empty_custom_strings_and_rejects_non_string_fields() {
    let request = json!({"model": "m", "instructions": "", "tools": [{"type": "custom", "name": "apply_patch"}], "input": [
        {"type": "custom_tool_call", "call_id": "call", "name": "apply_patch", "input": ""},
        {"type": "custom_tool_call_output", "call_id": "call", "output": ""}
    ]});
    assert_append_only(CacheProtocol::Codex, &request, &request).unwrap();
    for (index, field) in [
        (0, "call_id"),
        (0, "name"),
        (0, "input"),
        (1, "call_id"),
        (1, "output"),
    ] {
        for invalid in [Value::Null, json!(123)] {
            let mut malformed = request.clone();
            malformed["input"][index][field] = invalid;
            assert!(assert_append_only(CacheProtocol::Codex, &malformed, &malformed).is_err());
        }
    }
}
