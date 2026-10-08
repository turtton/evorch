use super::*;
use futures_util::StreamExt;

fn mcp_args(id: &str, values: Value) -> Proto {
    let mut args = Proto::new()
        .string(1, "lookup")
        .string(3, id)
        .string(5, "lookup");
    for (key, value) in values.as_object().unwrap() {
        args = args.message(
            2,
            Proto::new()
                .string(1, key)
                .message(2, wire::encode_value(value)),
        );
    }
    args
}
fn streamed(kind: u32, envelope: &str, args: Option<Proto>, partial: Option<&str>) -> Proto {
    let mut update = Proto::new().string(1, envelope);
    if let Some(args) = args {
        update = update.message(2, Proto::new().message(15, Proto::new().message(1, args)));
    }
    if let Some(partial) = partial {
        update = update.string(3, partial);
    }
    Proto::new().message(1, Proto::new().message(kind, update))
}
fn exec(id: u64, args: Proto) -> Proto {
    Proto::new().message(2, Proto::new().integer(1, id).message(11, args))
}

#[tokio::test]
async fn streamed_envelopes_and_completion_restore_omitted_exec_arguments_without_crossing_calls() {
    // Both orderings occur: completion may be queued until the exec handoff.
    for completion_after_exec in [false, true] {
        let server = MockServer::start().await;
        let large = "large argument ".repeat(8192);
        let partial=json!({"query":"partial","payload":{"text":large},"array":[1,{"nested":true}],"only_streamed":"kept"}).to_string();
        let completion = streamed(
            3,
            "envelope-A",
            Some(mcp_args(
                "",
                json!({"query":"complete","payload":"truncated object", "completion_only":{"done":true}}),
            )),
            None,
        );
        let args = mcp_args("tool-A", json!({"query":"exec"}))
            .message(
                2,
                Proto::new()
                    .string(1, "payload")
                    .bytes(2, b"raw truncated object"),
            )
            .message(
                2,
                Proto::new()
                    .string(1, "raw_text")
                    .bytes(2, b"plain raw string"),
            )
            .message(
                2,
                Proto::new()
                    .string(1, "raw_json")
                    .bytes(2, br#"{"from_raw":true}"#),
            )
            .message(
                2,
                Proto::new()
                    .string(1, "proto_object")
                    .message(2, wire::encode_value(&json!("{\"from_proto\":true}"))),
            )
            .message(
                2,
                Proto::new()
                    .string(1, "proto_array")
                    .message(2, wire::encode_value(&json!("[1,2]"))),
            );
        let mut frames = vec![
            // Partial data may arrive before a start frame binds the two IDs.
            streamed(7, "envelope-A", None, Some(&partial[..20])),
            streamed(
                2,
                "envelope-A",
                Some(mcp_args("tool-A", json!({"query":"start"}))),
                None,
            ),
            streamed(2, "envelope-B", Some(mcp_args("tool-B", json!({}))), None),
            streamed(7, "envelope-A", None, Some(&partial)),
            streamed(7, "envelope-A", None, Some(&partial)), // repeated cumulative snapshots
            streamed(
                7,
                "envelope-B",
                None,
                Some("{\"query\":\"B\",\"other\":12}"),
            ),
        ];
        if !completion_after_exec {
            frames.push(completion.clone());
        }
        frames.push(exec(1, args));
        frames.push(exec(2, mcp_args("tool-B", json!({}))));
        if completion_after_exec {
            frames.push(completion);
        }
        frames.push(streamed(
            3,
            "envelope-B",
            Some(mcp_args("tool-B", json!({"query":"B complete"}))),
            None,
        ));
        frames.push(end());
        Mock::given(path("/aiserver.v1.BidiService/BidiAppend"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(path("/agent.v1.AgentService/RunSSE"))
            .respond_with(response(frames))
            .mount(&server)
            .await;
        let mut stream = client(&server)
            .stream(&ProviderAuth::new(""), &request())
            .await
            .unwrap();
        let mut events = Vec::new();
        let mut completed = None;
        while let Some(event) = stream.next().await {
            let event = event.unwrap();
            if let StreamEvent::Completed { response } = &event {
                completed = Some(response.clone());
            }
            events.push(event);
        }
        let output = completed.unwrap();
        let expected_a = json!({"query":"complete","payload":{"text":large},"array":[1,{"nested":true}],"only_streamed":"kept","completion_only":{"done":true},"raw_text":"plain raw string","raw_json":{"from_raw":true},"proto_object":{"from_proto":true},"proto_array":[1,2]});
        let expected_b = json!({"query":"B complete","other":12});
        assert_eq!(
            output.message.content,
            [
                ContentBlock::ToolUse {
                    id: "tool-A".into(),
                    name: "lookup".into(),
                    input: expected_a.clone()
                },
                ContentBlock::ToolUse {
                    id: "tool-B".into(),
                    name: "lookup".into(),
                    input: expected_b.clone()
                }
            ]
        );
        for (index, expected) in [expected_a, expected_b].into_iter().enumerate() {
            let arguments = events
                .iter()
                .filter_map(|event| match event {
                    StreamEvent::ToolCallDelta {
                        index: i,
                        arguments_delta,
                        ..
                    } if *i == index => Some(arguments_delta.as_str()),
                    _ => None,
                })
                .collect::<String>();
            assert_eq!(
                serde_json::from_str::<Value>(&arguments).unwrap(),
                expected,
                "streamed argument JSON is published once, after authoritative completion"
            );
        }
    }
}

#[tokio::test]
async fn truncated_streamed_json_is_an_error_instead_of_executing_incomplete_input() {
    let server = MockServer::start().await;
    Mock::given(path("/aiserver.v1.BidiService/BidiAppend"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(path("/agent.v1.AgentService/RunSSE"))
        .respond_with(response(vec![
            streamed(2, "envelope", Some(mcp_args("tool", json!({}))), None),
            streamed(
                7,
                "envelope",
                None,
                Some("{\"query\":\"credential-sentinel"),
            ),
            exec(1, mcp_args("tool", json!({}))),
            end(),
        ]))
        .mount(&server)
        .await;
    let error = client(&server)
        .send(&ProviderAuth::new(""), &request())
        .await
        .unwrap_err();
    assert!(matches!(error, ProviderError::InvalidSse { .. }));
    assert!(!error.to_string().contains("credential-sentinel"));
}

#[tokio::test]
async fn unknown_connect_error_code_does_not_echo_credentials() {
    let server = MockServer::start().await;
    Mock::given(path("/aiserver.v1.BidiService/BidiAppend"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let mut bytes = wire::frame(br#"{"error":{"code":"credential-sentinel","message":"secret"}}"#);
    bytes[0] = 2;
    Mock::given(path("/agent.v1.AgentService/RunSSE"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(bytes, "application/connect+proto"))
        .mount(&server)
        .await;
    let error = client(&server)
        .send(&ProviderAuth::new(""), &request())
        .await
        .unwrap_err();
    assert_eq!(
        error,
        ProviderError::Request("Cursor Connect request failed".into())
    );
}
