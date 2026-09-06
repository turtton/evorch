use std::collections::BTreeSet;

use mock_openai::ScriptedResponse;
use serde_json::{Value, json};

fn payloads(body: &str) -> Vec<Value> {
    body.split("\n\n")
        .filter(|frame| !frame.is_empty())
        .filter_map(|frame| {
            let payload = frame
                .strip_prefix("data: ")
                .expect("SSE data prefix")
                .trim_end();
            (payload != "[DONE]").then(|| serde_json::from_str(payload).expect("JSON payload"))
        })
        .collect()
}

fn keys(value: &Value) -> BTreeSet<&str> {
    value
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect()
}

#[test]
fn text_stream_frames_match_wire_shape() {
    // Given
    let scenario = ScriptedResponse::text_stream("chatcmpl-1", "m", ["Hel", "lo"]);
    // When
    let frames = scenario.frames();
    let chunks = payloads(&frames.concat());
    // Then
    assert_eq!(frames.len(), 4);
    assert!(frames.iter().all(|frame| frame.ends_with("\n\n")));
    assert_eq!(frames.last().unwrap(), "data: [DONE]\n\n");
    assert_eq!(scenario.body(), frames.concat());
    assert_eq!(scenario.frames(), frames);
    assert_eq!(
        chunks[0]["choices"][0]["delta"],
        json!({"role":"assistant","content":"Hel"})
    );
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "lo");
    assert_eq!(chunks[1]["choices"][0]["finish_reason"], "stop");
    assert_eq!(chunks[2]["choices"], json!([]));
    assert!(chunks[2]["usage"]["total_tokens"].is_u64());
    for chunk in &chunks {
        assert_eq!(chunk["id"], "chatcmpl-1");
        assert_eq!(chunk["model"], "m");
        assert_eq!(chunk["object"], "chat.completion.chunk");
        assert_eq!(chunk["created"], chunks[0]["created"]);
        assert!(chunk["created"].is_u64());
    }
}

#[test]
fn tool_call_frames_split_arguments_by_index() {
    // Given
    let scenario = ScriptedResponse::tool_call(
        "chatcmpl-2",
        "m",
        0,
        "call_1",
        "edit",
        ["{\"path\":", "\"a.txt\"}"],
    );
    // When
    let frames = scenario.frames();
    let chunks = payloads(&frames.concat());
    // Then
    assert_eq!(frames.len(), 4);
    assert!(frames.iter().all(|frame| frame.ends_with("\n\n")));
    assert_eq!(frames.last().unwrap(), "data: [DONE]\n\n");
    let first = &chunks[0]["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(first["id"], "call_1");
    assert_eq!(first["type"], "function");
    assert_eq!(first["function"]["name"], "edit");
    let arguments: String = chunks[..2]
        .iter()
        .map(|chunk| {
            let call = &chunk["choices"][0]["delta"]["tool_calls"][0];
            assert_eq!(call["index"], 0);
            call["function"]["arguments"].as_str().unwrap()
        })
        .collect();
    assert_eq!(arguments, "{\"path\":\"a.txt\"}");
    assert!(
        chunks[1]["choices"][0]["delta"]["tool_calls"][0]
            .get("id")
            .is_none()
    );
    assert_eq!(chunks[1]["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(chunks[2]["choices"], json!([]));
    assert!(chunks[2]["usage"]["total_tokens"].is_u64());
}

#[test]
fn frames_shape_matches_fixture() {
    // Given
    let fixture = payloads(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../providers/tests/fixtures/openai/stream_tool_call.sse"
    )));
    let scenario = ScriptedResponse::tool_call(
        "chatcmpl-2",
        "m",
        0,
        "call_1",
        "edit",
        ["{\"path\":", "\"a.txt\"}"],
    );
    // When
    let chunks = payloads(&scenario.body());
    // Then: the accepted minimal fixture omits standard response metadata.
    let metadata = BTreeSet::from(["object", "created", "model"]);
    for (actual, accepted) in [
        (&chunks[0], &fixture[0]),
        (chunks.last().unwrap(), fixture.last().unwrap()),
    ] {
        let actual_keys = keys(actual);
        assert!(metadata.is_subset(&actual_keys));
        assert_eq!(
            actual_keys
                .difference(&metadata)
                .copied()
                .collect::<BTreeSet<_>>(),
            keys(accepted)
        );
    }
}

#[test]
fn non_streaming_json_roundtrip() {
    // Given
    let text = ScriptedResponse::text_stream("chatcmpl-1", "m", ["Hel", "lo"]).with_usage(13, 9);
    let tool = ScriptedResponse::tool_call(
        "chatcmpl-2",
        "m",
        0,
        "call_1",
        "edit",
        ["{\"path\":", "\"a.txt\"}"],
    );
    // When
    let responses = [text.to_non_streaming_json(), tool.to_non_streaming_json()]
        .map(|value| serde_json::from_str::<Value>(&value.to_string()).unwrap());
    // Then
    assert_eq!(responses[0]["object"], "chat.completion");
    assert_eq!(responses[0]["choices"][0]["message"]["content"], "Hello");
    assert_eq!(responses[0]["choices"][0]["finish_reason"], "stop");
    assert_eq!(
        responses[0]["usage"],
        json!({"prompt_tokens":13,"completion_tokens":9,"total_tokens":22})
    );
    assert_eq!(
        payloads(&text.body()).last().unwrap()["usage"],
        responses[0]["usage"]
    );
    assert_eq!(responses[1]["object"], "chat.completion");
    assert_eq!(
        responses[1]["choices"][0]["message"]["tool_calls"][0],
        json!({
            "id":"call_1", "type":"function", "function":{"name":"edit","arguments":"{\"path\":\"a.txt\"}"}
        })
    );
    assert_eq!(responses[1]["choices"][0]["finish_reason"], "tool_calls");
}
