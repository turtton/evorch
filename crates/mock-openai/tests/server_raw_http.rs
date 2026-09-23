use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use mock_openai::{ScriptedResponse, StreamingMockOpenAi, WriteMode};
use serde_json::{Value, json};

fn address(server: &StreamingMockOpenAi) -> String {
    server
        .base_url()
        .strip_prefix("http://")
        .unwrap()
        .strip_suffix("/v1")
        .unwrap()
        .to_owned()
}

fn post(server: &StreamingMockOpenAi, body: &Value) -> (String, String) {
    let mut stream = TcpStream::connect(address(server)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let body = body.to_string();
    write!(stream, "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer k\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}", body.len()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    (headers.to_owned(), body.to_owned())
}

fn get(server: &StreamingMockOpenAi, path: &str) -> (String, String) {
    let mut stream = TcpStream::connect(address(server)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    (headers.to_owned(), body.to_owned())
}

#[test]
fn serves_models_when_ids_are_configured() {
    for ids in [vec!["model-beta", "model-alpha"], vec![]] {
        // Given
        let server = StreamingMockOpenAi::spawn_with_models(
            vec![],
            WriteMode::FramePerWrite,
            ids.iter().map(|id| (*id).to_owned()).collect(),
        );
        // When
        let (headers, body) = get(&server, "/v1/models");
        // Then
        assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(
            headers
                .lines()
                .any(|line| line == "Content-Type: application/json")
        );
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["object"], "list");
        let models = body["data"].as_array().unwrap();
        assert_eq!(
            models
                .iter()
                .map(|model| model["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ids
        );
        for model in models {
            assert_eq!(model["object"], "model");
            assert_eq!(model["created"], 0);
            assert_eq!(model["owned_by"], "mock-openai");
        }
        let requests = server.recorded_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/v1/models");
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].body, Value::Null);
        assert!(!requests[0].stream);
    }
}

#[test]
fn serves_default_models_when_scripts_are_empty() {
    // Given
    let server = StreamingMockOpenAi::spawn(vec![]);
    // When
    let (_, body) = get(&server, "/v1/models");
    // Then
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({
            "object": "list",
            "data": [{"id": "mock-model", "object": "model", "created": 0, "owned_by": "mock-openai"}]
        })
    );
}

#[test]
fn preserves_chat_script_when_models_are_requested() {
    // Given
    let response = ScriptedResponse::text_stream("after-models", "m", ["hello"]);
    let server = StreamingMockOpenAi::spawn(vec![response.clone()]);
    // When
    get(&server, "/v1/models");
    // Then
    assert_eq!(server.remaining_scripts(), 1);
    assert_eq!(post(&server, &json!({"stream": true})).1, response.body());
    assert_eq!(server.recorded_requests()[1].method, "POST");
    assert_eq!(server.remaining_scripts(), 0);
}

#[test]
fn serves_sse_for_stream_true_and_records_request() {
    // Given
    let response = ScriptedResponse::text_stream("r1", "m", ["hello", " world"]);
    let server = StreamingMockOpenAi::spawn(vec![response.clone()]);
    // When
    let (headers, body) = post(&server, &json!({"model":"m","stream":true,"messages":[]}));
    // Then
    assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(
        headers
            .lines()
            .any(|line| line == "Content-Type: text/event-stream")
    );
    assert!(
        headers
            .lines()
            .any(|line| line == "Cache-Control: no-cache")
    );
    assert!(headers.lines().any(|line| line == "Connection: close"));
    assert!(!headers.to_ascii_lowercase().contains("transfer-encoding:"));
    assert_eq!(body, response.body());
    let requests = server.recorded_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].authorization.as_deref(), Some("Bearer k"));
    assert!(requests[0].stream);
    assert_eq!(requests[0].body["model"], "m");
    assert_eq!(requests[0].path, "/v1/chat/completions");
}

#[test]
fn serves_json_when_stream_false() {
    for request in [
        json!({"model":"m","stream":false,"messages":[]}),
        json!({"model":"m","messages":[]}),
    ] {
        // Given
        let response = ScriptedResponse::text_stream("json", "m", ["こんにちは"]).with_usage(3, 5);
        let server = StreamingMockOpenAi::spawn(vec![response.clone()]);
        // When
        let (headers, body) = post(&server, &request);
        // Then
        assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(
            headers
                .lines()
                .any(|line| line == "Content-Type: application/json")
        );
        assert!(
            headers
                .lines()
                .any(|line| line == format!("Content-Length: {}", body.len()))
        );
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            response.to_non_streaming_json()
        );
        assert!(!server.recorded_requests()[0].stream);
    }
}

#[test]
fn pops_scripts_in_order_and_500s_when_exhausted() {
    // Given
    let first = ScriptedResponse::text_stream("first", "m", ["one"]);
    let second = ScriptedResponse::text_stream("second", "m", ["two"]);
    let server = StreamingMockOpenAi::spawn(vec![first.clone(), second.clone()]);
    let request = json!({"model":"m","stream":true,"messages":[]});
    // When
    let responses: Vec<_> = (0..3).map(|_| post(&server, &request)).collect();
    // Then
    assert_eq!(responses[0].1, first.body());
    assert_eq!(responses[1].1, second.body());
    assert!(
        responses[2]
            .0
            .starts_with("HTTP/1.1 500 Internal Server Error\r\n")
    );
    assert!(
        responses[2]
            .0
            .lines()
            .any(|line| line == "Content-Type: application/json")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&responses[2].1).unwrap(),
        json!({"error":{"message":"unscripted request","type":"mock_openai"}})
    );
    assert_eq!(server.recorded_requests().len(), 3);
    assert_eq!(server.recorded_requests()[2].body, request);
    assert_eq!(server.remaining_scripts(), 0);
}

#[test]
fn frame_per_write_body_is_identical_to_whole_body() {
    // Given
    let response =
        ScriptedResponse::tool_call("tool", "m", 0, "call", "search", ["{\"q\":", "\"rust\"}"]);
    let frames = StreamingMockOpenAi::spawn_with(vec![response.clone()], WriteMode::FramePerWrite);
    let whole = StreamingMockOpenAi::spawn_with(vec![response.clone()], WriteMode::WholeBody);
    let request = json!({"model":"m","stream":true,"messages":[]});
    // When
    let bodies = [post(&frames, &request).1, post(&whole, &request).1];
    // Then
    assert_eq!(bodies[0].as_bytes(), bodies[1].as_bytes());
    assert_eq!(bodies[0], response.body());
}

#[test]
fn drop_shuts_down_listener() {
    // Given
    let server = StreamingMockOpenAi::spawn(vec![]);
    let address = address(&server);
    // When
    drop(server);
    // Then
    assert!(TcpStream::connect(address).is_err());
}

fn prompt_cache_usage(server: &StreamingMockOpenAi, request: &Value) -> (u64, u64) {
    let (headers, body) = post(server, request);
    assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
    let usage = if request["stream"] == true {
        body.split("\n\n")
            .filter_map(|frame| frame.strip_prefix("data: "))
            .filter(|frame| *frame != "[DONE]")
            .map(|frame| serde_json::from_str::<Value>(frame).unwrap())
            .find_map(|chunk| chunk.get("usage").cloned())
            .expect("SSE usage frame")
    } else {
        serde_json::from_str::<Value>(&body).unwrap()["usage"].clone()
    };
    let input = usage["prompt_tokens"].as_u64().unwrap();
    let cached = usage["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap();
    assert!(cached <= input);
    assert_eq!(usage["completion_tokens"], 7);
    assert_eq!(usage["total_tokens"], input + 7);
    (input, cached)
}

fn cache_scripts(count: usize) -> Vec<ScriptedResponse> {
    (0..count)
        .map(|index| {
            ScriptedResponse::text_stream(&format!("cache-{index}"), "m", ["ok"]).with_usage(999, 7)
        })
        .collect()
}

#[test]
fn prompt_cache_reuses_append_loses_rewritten_history_and_rewarms() {
    for streaming in [false, true] {
        let server = StreamingMockOpenAi::spawn_with_prompt_cache(cache_scripts(4));
        let mut request = json!({
            "model": "m", "stream": streaming,
            "messages": [
                {"role": "system", "content": "Keep the existing transcript."},
                {"role": "user", "content": "Read the tool result."},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call-1", "type": "function",
                    "function": {"name": "read", "arguments": "{}"}
                }]},
                {"role": "tool", "tool_call_id": "call-1", "content": "large output ".repeat(200)}
            ]
        });
        let (first_input, first_cached) = prompt_cache_usage(&server, &request);
        assert_eq!(first_cached, 0);

        request["messages"].as_array_mut().unwrap().extend([
            json!({"role": "assistant", "content": "Read."}),
            json!({"role": "user", "content": "Continue."}),
        ]);
        let (appended_input, appended_cached) = prompt_cache_usage(&server, &request);
        assert!(appended_input > first_input);
        assert_eq!(appended_cached, first_input);

        // Retroactive tool-result replacement breaks the reusable prefix.
        request["messages"][3]["content"] = json!("Full output: /tmp/output.txt");
        let (rewritten_input, rewritten_cached) = prompt_cache_usage(&server, &request);
        assert!(rewritten_input < appended_input);
        assert!(rewritten_cached < first_input);
        assert!(rewritten_cached < rewritten_input);

        // The new transcript becomes cacheable normally; compaction needs no bypass.
        request["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role": "assistant", "content": "Continued."}));
        let (_, warmed_cached) = prompt_cache_usage(&server, &request);
        assert_eq!(warmed_cached, rewritten_input);
        assert_eq!(server.remaining_scripts(), 0);
    }
}

#[test]
fn prompt_cache_is_shared_between_json_and_sse_transport() {
    let server = StreamingMockOpenAi::spawn_with_prompt_cache(cache_scripts(3));
    let mut request = json!({"model": "m", "messages": [{"role": "user", "content": "hello"}]});
    let (input, cached) = prompt_cache_usage(&server, &request);
    assert_eq!(cached, 0);
    request["stream"] = json!(true);
    request["stream_options"] = json!({"include_usage": true});
    assert_eq!(prompt_cache_usage(&server, &request), (input, input));
    request["stream"] = json!(false);
    assert_eq!(prompt_cache_usage(&server, &request), (input, input));
}

#[test]
fn prompt_cache_isolates_models_and_cache_keys() {
    let server = StreamingMockOpenAi::spawn_with_prompt_cache(cache_scripts(7));
    let original = json!({"model": "m", "messages": [{"role": "user", "content": "hello"}]});
    let (input, cached) = prompt_cache_usage(&server, &original);
    assert_eq!(cached, 0);
    let mut request = original.clone();
    request["model"] = json!("other-model");
    assert_eq!(prompt_cache_usage(&server, &request).1, 0);
    request = original.clone();
    request["prompt_cache_key"] = json!("thread-a");
    let (keyed_input, keyed_cached) = prompt_cache_usage(&server, &request);
    assert_eq!(keyed_cached, 0);
    request["prompt_cache_key"] = json!("thread-b");
    assert_eq!(prompt_cache_usage(&server, &request).1, 0);
    request["prompt_cache_key"] = json!("thread-a");
    assert_eq!(
        prompt_cache_usage(&server, &request),
        (keyed_input, keyed_input)
    );
    assert_eq!(prompt_cache_usage(&server, &original), (input, input));
    request["model"] = json!("other-model");
    assert_eq!(prompt_cache_usage(&server, &request).1, 0);
}

#[test]
fn prompt_cache_includes_tool_definitions_and_other_request_settings() {
    for changed_field in ["tools", "temperature", "response_format"] {
        let server = StreamingMockOpenAi::spawn_with_prompt_cache(cache_scripts(2));
        let mut request = json!({
            "model": "m", "temperature": 0,
            "tools": [{"type": "function", "function": {"name": "read", "description": "Read data"}}],
            "response_format": {"type": "text"},
            "messages": [{"role": "user", "content": "hello"}]
        });
        let (input, _) = prompt_cache_usage(&server, &request);
        match changed_field {
            "tools" => request["tools"][0]["function"]["description"] = json!("Changed tool"),
            "temperature" => request["temperature"] = json!(1),
            "response_format" => request["response_format"]["type"] = json!("json_object"),
            _ => unreachable!(),
        }
        assert!(prompt_cache_usage(&server, &request).1 < input);
    }
}
