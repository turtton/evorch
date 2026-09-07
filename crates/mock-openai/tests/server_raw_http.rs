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
