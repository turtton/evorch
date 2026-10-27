use super::*;

#[tokio::test]
async fn unknown_server_fields_do_not_abort_a_valid_turn() {
    // Given: a newer server sends an unknown protobuf field before and after text.
    let server = MockServer::start().await;
    let unknown = Proto::new().message(99, Proto::new().string(1, "opaque extension"));
    let text = Proto::new().message(1, Proto::new().message(1, Proto::new().string(1, "Hello")));
    Mock::given(path("/agent.v1.AgentService/RunSSE"))
        .respond_with(response(vec![unknown.clone(), text, unknown, end()]))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/aiserver.v1.BidiService/BidiAppend"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    // When: the stream is collected through the normal provider API.
    let result = client(&server)
        .send(&ProviderAuth::new("unused"), &request())
        .await
        .unwrap();

    // Then: unknown data is not emitted as content and the turn ends normally.
    assert_eq!(result.finish_reason, FinishReason::Stop);
    assert_eq!(
        result.message.content,
        [ContentBlock::Text {
            text: "Hello".into()
        }]
    );
    assert_eq!(result.usage.input_tokens, 100);
    server.verify().await;
}

#[tokio::test]
async fn native_connect_handoff_preserves_tool_results_rules_and_conversation_checkpoint() {
    let server = MockServer::start().await;
    let args = Proto::new()
        .string(1, "lookup")
        .string(3, "call_1")
        .string(5, "lookup")
        .message(
            2,
            Proto::new()
                .string(1, "query")
                .message(2, wire::encode_value(&json!("needle"))),
        );
    let set = Proto::new().message(
        4,
        Proto::new()
            .integer(1, 20)
            .message(3, Proto::new().bytes(1, b"server-blob").bytes(2, b"saved")),
    );
    let get = Proto::new().message(
        4,
        Proto::new()
            .integer(1, 21)
            .message(2, Proto::new().bytes(1, b"server-blob")),
    );
    let checkpoint = Proto::new().message(
        3,
        Proto::new()
            .bytes(1, b"server-placeholder")
            .bytes(12, b"opaque-side-state")
            .string(4, "stale pending call"),
    );
    let context = Proto::new().message(2, Proto::new().integer(1, 1).message(10, Proto::new()));
    let native = Proto::new().message(
        2,
        Proto::new()
            .integer(1, 2)
            .message(7, Proto::new().string(1, "/must-not-read")),
    );
    let tool = Proto::new().message(
        2,
        Proto::new()
            .integer(1, 3)
            .string(15, "exec-3")
            .message(11, args),
    );
    Mock::given(path("/agent.v1.AgentService/RunSSE"))
        .respond_with(response(vec![
            set,
            get,
            context.clone(),
            native,
            tool,
            checkpoint,
            end(),
        ]))
        .up_to_n_times(1)
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/aiserver.v1.BidiService/BidiAppend"))
        .and(header("content-type", "application/proto"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let client = client(&server);
    let mut request = request();
    let response1 = client
        .send(&ProviderAuth::new("unused"), &request)
        .await
        .unwrap();
    assert_eq!(response1.finish_reason, FinishReason::ToolUse);
    assert_eq!(response1.usage.input_tokens, 100);
    assert_eq!(response1.usage.cache_read_tokens, 75);
    assert_eq!(
        response1.message.content,
        [ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "lookup".into(),
            input: json!({"query":"needle"})
        }]
    );
    request.messages.push(response1.message);
    request.messages.push(Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_call_id: "call_1".into(),
            content: vec![ToolResultContent::Text {
                text: "The answer".into(),
            }],
            is_error: false,
        }],
    });
    Mock::given(path("/agent.v1.AgentService/RunSSE"))
        .respond_with(response(vec![
            context,
            Proto::new().message(1, Proto::new().message(1, Proto::new().string(1, "Done"))),
            end(),
        ]))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        client
            .send(&ProviderAuth::new("unused"), &request)
            .await
            .unwrap()
            .finish_reason,
        FinishReason::Stop
    );
    let requests = server.received_requests().await.unwrap();
    let appends: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path().ends_with("/BidiAppend"))
        .map(|r| r.body.as_slice())
        .collect();
    let runs: Vec<_> = appends
        .iter()
        .filter_map(|body| wire::nested(child(body, 4), 1).unwrap())
        .collect();
    assert_eq!(runs.len(), 2);
    assert_eq!(
        wire::string(runs[0], 5).unwrap(),
        wire::string(runs[1], 5).unwrap()
    );
    assert!(
        wire::nested(child(runs[1], 2), 2).unwrap().is_some(),
        "tool result uses resumeAction"
    );
    let state = child(runs[1], 1);
    assert_eq!(child(state, 12), b"opaque-side-state");
    assert!(wire::nested(state, 4).unwrap().is_none());
    let sessions = client.sessions.lock().await;
    let stored = sessions.values().next().unwrap().lock().await;
    let roots: Vec<Value> = wire::fields(state)
        .unwrap()
        .iter()
        .filter(|f| f.number == 1)
        .map(|f| serde_json::from_slice(&stored.blobs[f.data]).unwrap())
        .collect();
    assert_eq!(roots[1]["content"][0]["text"], "Use lookup");
    assert_eq!(roots[2]["content"][0]["toolCallId"], "call_1");
    assert_eq!(roots[3]["content"][0]["result"], "The answer");
    let contexts: Vec<_> = appends
        .iter()
        .filter_map(|a| wire::nested(child(a, 4), 2).unwrap())
        .filter_map(|e| wire::nested(e, 10).unwrap())
        .collect();
    assert_eq!(contexts.len(), 2);
    assert_eq!(
        contexts[0], contexts[1],
        "rules/tool schemas stable across tool continuation"
    );
    let ctx = child(child(contexts[0], 1), 1);
    assert_eq!(
        wire::string(child(ctx, 2), 2).unwrap(),
        "System instructions"
    );
    assert_eq!(
        wire::decode_value(child(child(ctx, 7), 3)).unwrap(),
        request.tools[0].input_schema
    );
    assert!(
        appends
            .iter()
            .any(|a| wire::nested(child(a, 4), 5).unwrap().is_some()),
        "native request gets explicit unsupported response"
    );
    assert!(
        appends
            .iter()
            .any(|a| wire::nested(child(a, 4), 3)
                .unwrap()
                .is_some_and(|kv| wire::nested(kv, 2)
                    .unwrap()
                    .is_some_and(|get| child(get, 1) == b"saved"))),
        "blob set/get handshake"
    );
    let mut seq_by_id = std::collections::HashMap::new();
    for body in appends {
        let id = wire::string(child(body, 2), 1).unwrap();
        let next = seq_by_id.entry(id).or_insert(0);
        assert_eq!(wire::integer(body, 3).unwrap(), Some(*next));
        *next += 1;
    }
}

#[tokio::test]
async fn checkpoint_token_details_report_context_occupancy_apart_from_turn_input() {
    // Given: a turn whose reported input (100) sums its model calls, and a
    // checkpoint whose tokenDetails.usedTokens (31) is the conversation's occupancy.
    let server = MockServer::start().await;
    let text = Proto::new().message(1, Proto::new().message(1, Proto::new().string(1, "Hi")));
    let checkpoint = Proto::new().message(
        3,
        Proto::new().message(5, Proto::new().integer(1, 31).integer(2, 200)),
    );
    Mock::given(path("/agent.v1.AgentService/RunSSE"))
        .respond_with(response(vec![text, checkpoint, end()]))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/aiserver.v1.BidiService/BidiAppend"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    // When
    let result = client(&server)
        .send(&ProviderAuth::new("unused"), &request())
        .await
        .unwrap();

    // Then: the turn's usage is kept for accounting and the occupancy is reported apart.
    assert_eq!(result.usage.input_tokens, 100);
    assert_eq!(result.usage.context_tokens, Some(31));
}
