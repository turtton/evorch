use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tools::mcp::{
    McpClient, McpClientInfo, McpError, McpErrorKind, McpToolDefinition,
    codec::{SseDecoder, parse_frame},
    headers,
    protocol::{ProtocolEra, ProtocolVersion},
    transport::api::*,
};
#[allow(dead_code)]
#[path = "../../shared-test-support/mcp/mod.rs"]
mod golden;
struct Fake {
    steps: Mutex<VecDeque<golden::Step>>,
    requests: Mutex<Vec<Value>>,
    responses: Mutex<Vec<Value>>,
    kind: TransportKind,
    mapping: Mutex<Vec<headers::ParameterHeader>>,
}
impl Fake {
    fn script(steps: Vec<golden::Step>, kind: TransportKind) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into()),
            requests: Mutex::new(vec![]),
            responses: Mutex::new(vec![]),
            kind,
            mapping: Mutex::new(vec![]),
        })
    }
    fn next(&self) -> golden::Step {
        self.steps.lock().unwrap().pop_front().expect("script step")
    }
    fn send(&self, value: Value) {
        self.requests.lock().unwrap().push(value.clone());
        let golden::Step::Send { value: expected } = self.next() else {
            panic!("expected send")
        };
        assert_eq!(value, expected.get("body").unwrap_or(&expected).clone());
    }
    fn receive(&self) -> McpResponseEnvelope {
        let golden::Step::Receive { value } = self.next() else {
            panic!("expected receive")
        };
        let is_http = value.get("status").is_some();
        let body = if is_http { &value["body"] } else { &value };
        let mut frames = vec![];
        let mut bytes = 0;
        if let Some(chunks) = value.get("chunks").and_then(Value::as_array) {
            let mut decoder = SseDecoder::default();
            for chunk in chunks {
                frames.extend(decoder.push(chunk.as_str().unwrap().as_bytes()).unwrap());
            }
            bytes = decoder.received_bytes();
        } else if !body.is_null() {
            let data = serde_json::to_vec(body).unwrap();
            bytes = data.len();
            frames.push(parse_frame(&data).unwrap());
        }
        if let Some(tools) = body
            .get("result")
            .and_then(|r| r.get("tools"))
            .and_then(Value::as_array)
        {
            let mut out = vec![];
            for tool in tools {
                if let Ok(tool) = serde_json::from_value::<McpToolDefinition>(tool.clone())
                    && let Ok(mapping) = headers::parameter_headers(&tool)
                {
                    out = mapping;
                }
            }
            *self.mapping.lock().unwrap() = out;
        }
        McpResponseEnvelope::new(
            value
                .get("status")
                .and_then(Value::as_u64)
                .map(|s| s as u16),
            McpResponseHeaders::default(),
            frames,
            bytes,
        )
        .unwrap()
    }
    fn done(&self) {
        while let Some(step) = self.steps.lock().unwrap().pop_front() {
            assert!(matches!(
                step,
                golden::Step::Close | golden::Step::Signal { .. } | golden::Step::Gate { .. }
            ));
        }
    }
}
#[async_trait]
impl McpTransport for Fake {
    async fn exchange(
        &self,
        request: RpcRequest,
        _cancel: CancelContext,
    ) -> Result<McpResponseEnvelope, McpError> {
        let body = serde_json::to_value(&request).unwrap();
        if self.kind == TransportKind::Http {
            // Expected headers are authored independently in the fixture. Legacy
            // session headers are verified against real HTTP in mcp_http instead.
            let expected = self.steps.lock().unwrap().front().cloned().unwrap();
            if let golden::Step::Send { value } = expected
                && request.params.get("_meta").is_some()
            {
                let actual =
                    headers::request_headers(&body, &self.mapping.lock().unwrap()).unwrap();
                for (key, value) in value["headers"].as_object().unwrap() {
                    assert_eq!(
                        actual.get(key).unwrap().to_str().unwrap(),
                        value.as_str().unwrap()
                    );
                }
            }
        }
        self.send(body);
        Ok(self.receive())
    }
    async fn notify(&self, notification: RpcNotification) -> Result<(), McpError> {
        self.send(serde_json::to_value(notification).unwrap());
        if self.kind == TransportKind::Http {
            self.receive();
        }
        Ok(())
    }
    async fn respond(&self, response: RpcFrame) -> Result<(), McpError> {
        self.responses
            .lock()
            .unwrap()
            .push(serde_json::to_value(response).unwrap());
        Ok(())
    }
    async fn cancel(&self, _request: CancelRequest) -> Result<(), McpError> {
        Ok(())
    }
    async fn shutdown(&self, _reason: ShutdownReason) -> Result<(), McpError> {
        Ok(())
    }
}
fn info() -> McpClientInfo {
    McpClientInfo {
        name: "evorch".into(),
        version: "0.1.0".into(),
    }
}
async fn connect(fake: Arc<dyn McpTransport>, kind: TransportKind) -> Result<McpClient, McpError> {
    McpClient::connect_transport(
        fake,
        "fixture".into(),
        kind,
        info(),
        Duration::from_secs(10),
        CancelContext::default(),
    )
    .await
}
async fn transcript(name: &str, kind: TransportKind) {
    let path = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("fixtures/mcp")
        .join(name);
    let fake = Fake::script(golden::load(&path), kind);
    let mut client = connect(fake.clone(), kind).await.unwrap();
    let modern = name.starts_with("modern");
    assert_eq!(
        client.era(),
        if modern {
            ProtocolEra::Modern
        } else {
            ProtocolEra::Legacy
        }
    );
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.len(), 1);
    let args = if modern && kind == TransportKind::Http {
        json!({"text":"hello","region":"west"})
    } else {
        json!({"text":"hello"})
    };
    let result = client.call_tool("echo", args).await.unwrap();
    assert_eq!(result.text(), "hello");
    fake.done();
}
#[tokio::test]
async fn golden_modern_http() {
    transcript("modern-http.json", TransportKind::Http).await;
}
#[tokio::test]
async fn golden_legacy_http() {
    transcript("legacy-http.json", TransportKind::Http).await;
}
fn send(value: Value) -> golden::Step {
    golden::Step::Send { value }
}
fn receive(value: Value) -> golden::Step {
    golden::Step::Receive { value }
}
fn meta() -> Value {
    json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{},"io.modelcontextprotocol/clientInfo":{"name":"evorch","version":"0.1.0"}})
}
fn probe(id: i64) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"server/discover","params":{"_meta":meta()}})
}
fn discovered(id: i64) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private"}})
}
fn legacy_steps() -> Vec<golden::Step> {
    vec![
        send(
            json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"evorch","version":"0.1.0"}}}),
        ),
        receive(
            json!({"jsonrpc":"2.0","id":2,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}}),
        ),
        send(json!({"jsonrpc":"2.0","method":"notifications/initialized"})),
        golden::Step::Close,
    ]
}
#[tokio::test]
async fn discover_unknown_error_falls_back() {
    let mut steps = vec![
        send(probe(1)),
        receive(json!({"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"uninitialized"}})),
    ];
    steps.extend(legacy_steps());
    let fake = Fake::script(steps, TransportKind::Stdio);
    let client = connect(fake.clone(), TransportKind::Stdio).await.unwrap();
    assert_eq!(client.version(), ProtocolVersion::V2025_06_18);
    fake.done();
}
#[tokio::test]
async fn modern_400_error_does_not_fall_back() {
    let fake = Fake::script(
        vec![
            send(probe(1)),
            receive(
                json!({"status":400,"body":{"jsonrpc":"2.0","id":1,"error":{"code":-32022,"message":"unsupported","data":{"requested":"1900-01-01","supported":["2026-07-28"]}}}}),
            ),
            send(probe(2)),
            receive(json!({"status":200,"body":discovered(2)})),
            golden::Step::Close,
        ],
        TransportKind::Stdio,
    );
    let client = connect(fake.clone(), TransportKind::Http).await.unwrap();
    assert_eq!(client.era(), ProtocolEra::Modern);
    fake.done();
}
#[tokio::test]
async fn input_required_is_not_replayed() {
    let fake = Fake::script(
        vec![
            send(probe(1)),
            receive(discovered(1)),
            send(
                json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"login","arguments":{},"_meta":meta()}}),
            ),
            receive(
                json!({"jsonrpc":"2.0","id":2,"result":{"resultType":"input_required","inputRequests":{},"requestState":"opaque"}}),
            ),
            golden::Step::Close,
        ],
        TransportKind::Stdio,
    );
    let mut client = connect(fake.clone(), TransportKind::Stdio).await.unwrap();
    assert_eq!(
        client.call_tool("login", json!({})).await.unwrap_err().kind,
        McpErrorKind::InputRequired
    );
    fake.done();
}
#[test]
fn modern_headers_and_meta_agree() {
    let body = json!({"method":"tools/call","params":{"name":"remote original/名前","arguments":{},"_meta":meta()}});
    let actual = headers::request_headers(&body, &[]).unwrap();
    assert_eq!(
        actual["mcp-protocol-version"],
        body["params"]["_meta"][headers::VERSION_META]
            .as_str()
            .unwrap()
    );
    assert_eq!(actual["mcp-method"], "tools/call");
    assert_eq!(
        actual["mcp-name"],
        headers::encode_value("remote original/名前").unwrap()
    );
}
fn tool(schema: Value) -> McpToolDefinition {
    McpToolDefinition {
        name: "tool".into(),
        description: None,
        input_schema: schema.as_object().unwrap().clone(),
    }
}
#[test]
fn x_mcp_header_mapping_is_validated() {
    for schema in [
        json!({"type":"object","properties":{"a":{"type":"string","x-mcp-header":""}}}),
        json!({"type":"object","properties":{"a":{"type":"number","x-mcp-header":"A"}}}),
        json!({"type":"object","properties":{"a":{"type":"string","x-mcp-header":"A"},"b":{"type":"boolean","x-mcp-header":"a"}}}),
        json!({"type":"object","properties":{"a":{"type":"array","items":{"type":"string","x-mcp-header":"A"}}}}),
        json!({"type":"object","oneOf":[{"properties":{"a":{"type":"string","x-mcp-header":"A"}}}]}),
        json!({"type":"object","properties":{"a":{"type":"string","x-mcp-header":"bad\r\n"}}}),
    ] {
        assert!(headers::parameter_headers(&tool(schema)).is_err());
    }
    let definition = tool(
        json!({"type":"object","properties":{"nested":{"type":"object","properties":{"a":{"type":"string","x-mcp-header":"A"},"i":{"type":"integer","x-mcp-header":"I"},"b":{"type":"boolean","x-mcp-header":"B"}}}}}),
    );
    let mappings = headers::parameter_headers(&definition).unwrap();
    let mut body = json!({"method":"tools/call","params":{"name":"tool","arguments":{"nested":{"a":"=?base64?literal?=","i":9007199254740991_i64,"b":true}},"_meta":meta()}});
    let actual = headers::request_headers(&body, &mappings).unwrap();
    assert_eq!(actual["mcp-param-a"], "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?=");
    assert_eq!(actual["mcp-param-i"], "9007199254740991");
    assert_eq!(actual["mcp-param-b"], "true");
    body["params"]["arguments"]["nested"]["i"] = json!(9007199254740992_i64);
    assert!(headers::request_headers(&body, &mappings).is_err());
    body["params"]["arguments"] = json!({"nested":{"a":null}});
    let actual = headers::request_headers(&body, &mappings).unwrap();
    assert!(!actual.contains_key("mcp-param-a"));
    assert!(!actual.contains_key("mcp-param-i"));
    assert_eq!(
        headers::encode_value("line1\nline2").unwrap(),
        "=?base64?bGluZTEKbGluZTI=?="
    );
}
#[tokio::test]
async fn invalid_schema_excludes_only_one_tool() {
    let fake = Fake::script(
        vec![
            send(probe(1)),
            receive(discovered(1)),
            send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"_meta":meta()}})),
            receive(
                json!({"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","ttlMs":0,"cacheScope":"private","tools":[{"name":"bad","inputSchema":{"type":"nonsense"}},{"name":"good","inputSchema":{"type":"object","properties":{"id":{"type":"integer"}}}},{"name":"remote_ref","inputSchema":{"type":"object","$ref":"http://127.0.0.1/private"}}]}}),
            ),
            golden::Step::Close,
        ],
        TransportKind::Stdio,
    );
    let mut client = connect(fake.clone(), TransportKind::Http).await.unwrap();
    let tools = client.list_tools().await.unwrap();
    assert_eq!(
        tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        vec!["good"]
    );
    fake.done();
}
#[test]
fn codec_rejects_ambiguous_frames_and_handles_split_sse() {
    for body in [
        json!([]),
        json!({"jsonrpc":"1.0","id":1,"result":{}}),
        json!({"jsonrpc":"2.0","id":1,"result":{},"error":{"code":1,"message":"bad"}}),
        json!({"jsonrpc":"2.0","id":1.5,"result":{}}),
        json!({"jsonrpc":"2.0","result":{}}),
    ] {
        assert!(parse_frame(&serde_json::to_vec(&body).unwrap()).is_err());
    }
    let bytes =
        b"\xef\xbb\xbf: keepalive\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\r\n\r\n";
    let mut decoder = SseDecoder::default();
    let mut frames = vec![];
    for byte in bytes {
        frames.extend(decoder.push(&[*byte]).unwrap());
    }
    assert_eq!(frames.len(), 1);
}

struct LateProbe {
    issued: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    seen: Mutex<Vec<Value>>,
}
#[async_trait]
impl McpTransport for LateProbe {
    async fn exchange(
        &self,
        request: RpcRequest,
        _cancel: CancelContext,
    ) -> Result<McpResponseEnvelope, McpError> {
        self.seen
            .lock()
            .unwrap()
            .push(serde_json::to_value(&request).unwrap());
        if request.method == "server/discover" {
            self.issued
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            return std::future::pending().await;
        }
        assert_eq!(request.id, RpcId::Number(2));
        let frames=vec![parse_frame(&serde_json::to_vec(&discovered(1)).unwrap()).unwrap(),parse_frame(&serde_json::to_vec(&json!({"jsonrpc":"2.0","id":2,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}})).unwrap()).unwrap()];
        Ok(McpResponseEnvelope::new(None, Default::default(), frames, 512).unwrap())
    }
    async fn notify(&self, notification: RpcNotification) -> Result<(), McpError> {
        self.seen
            .lock()
            .unwrap()
            .push(serde_json::to_value(notification).unwrap());
        Ok(())
    }
    async fn respond(&self, _: RpcFrame) -> Result<(), McpError> {
        panic!("no server requests")
    }
    async fn cancel(&self, _: CancelRequest) -> Result<(), McpError> {
        panic!("probe expiry is not cancellation")
    }
    async fn shutdown(&self, _: ShutdownReason) -> Result<(), McpError> {
        Ok(())
    }
}
#[tokio::test(start_paused = true)]
async fn discover_timeout_ignores_late_response() {
    let (issued, ready) = tokio::sync::oneshot::channel();
    let fake = Arc::new(LateProbe {
        issued: Mutex::new(Some(issued)),
        seen: Mutex::new(vec![]),
    });
    let client = {
        let fake = fake.clone();
        tokio::spawn(async move { connect(fake, TransportKind::Stdio).await })
    };
    ready.await.unwrap();
    tokio::time::advance(Duration::from_secs(11)).await;
    let client = client.await.unwrap().unwrap();
    assert_eq!(client.era(), ProtocolEra::Legacy);
    assert_eq!(client.version(), ProtocolVersion::V2024_11_05);
    assert_eq!(fake.seen.lock().unwrap().len(), 3);
}
#[tokio::test]
async fn modern_result_type_and_cache_fields_fail_closed() {
    let baseline = discovered(1);
    for result in [
        json!({"supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private"}),
        json!({"resultType":"unknown","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private"}),
        json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":-1,"cacheScope":"private"}),
        json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"shared"}),
    ] {
        let mut frame = baseline.clone();
        frame["result"] = result;
        let fake = Fake::script(
            vec![send(probe(1)), receive(frame), golden::Step::Close],
            TransportKind::Stdio,
        );
        let error = connect(fake.clone(), TransportKind::Stdio)
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind, McpErrorKind::Protocol);
        fake.done();
    }
}
#[tokio::test]
async fn list_limits_count_excluded_tools_and_duplicate_names() {
    for (tools, expected) in [
        (
            vec![
                json!({"name":"same","inputSchema":{}}),
                json!({"name":"same","inputSchema":{"type":"object"}}),
            ],
            McpErrorKind::Protocol,
        ),
        (
            (0..1001)
                .map(|i| json!({"name":format!("bad{i}"),"inputSchema":{"type":"nonsense"}}))
                .collect(),
            McpErrorKind::LimitExceeded,
        ),
    ] {
        let fake = Fake::script(
            vec![
                send(probe(1)),
                receive(discovered(1)),
                send(
                    json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"_meta":meta()}}),
                ),
                receive(
                    json!({"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","ttlMs":0,"cacheScope":"private","tools":tools}}),
                ),
                golden::Step::Close,
            ],
            TransportKind::Stdio,
        );
        let mut client = connect(fake.clone(), TransportKind::Stdio).await.unwrap();
        assert_eq!(client.list_tools().await.unwrap_err().kind, expected);
        fake.done();
    }
}
#[tokio::test]
async fn complete_page_limit_succeeds_but_more_pages_do_not() {
    for overflowing in [false, true] {
        let mut steps = vec![send(probe(1)), receive(discovered(1))];
        for page in 0..100 {
            let mut params = json!({"_meta":meta()});
            if page > 0 {
                params["cursor"] = json!(format!("cursor{page}"));
            }
            steps.push(send(
                json!({"jsonrpc":"2.0","id":page+2,"method":"tools/list","params":params}),
            ));
            let mut result =
                json!({"resultType":"complete","ttlMs":0,"cacheScope":"private","tools":[]});
            if page < 99 || overflowing {
                result["nextCursor"] = json!(format!("cursor{}", page + 1));
            }
            steps.push(receive(
                json!({"jsonrpc":"2.0","id":page+2,"result":result}),
            ));
        }
        steps.push(golden::Step::Close);
        let fake = Fake::script(steps, TransportKind::Stdio);
        let mut client = connect(fake.clone(), TransportKind::Stdio).await.unwrap();
        let result = client.list_tools().await;
        if overflowing {
            assert_eq!(result.unwrap_err().kind, McpErrorKind::LimitExceeded);
        } else {
            assert!(result.unwrap().is_empty());
        }
        fake.done();
    }
}
#[tokio::test]
async fn tool_error_keeps_all_supported_content_without_resource_fetch() {
    let content = json!([{"type":"text","text":"rejected"},{"type":"image","data":"AA==","mimeType":"image/png"},{"type":"audio","data":"AA==","mimeType":"audio/wav"},{"type":"resource_link","uri":"http://169.254.169.254/secret","name":"reference"},{"type":"resource","resource":{"uri":"file:///secret","text":"embedded"}}]);
    let fake = Fake::script(
        vec![
            send(probe(1)),
            receive(discovered(1)),
            send(
                json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tool","arguments":{},"_meta":meta()}}),
            ),
            receive(
                json!({"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","isError":true,"content":content,"structuredContent":{"error":"safe"}}}),
            ),
            golden::Step::Close,
        ],
        TransportKind::Stdio,
    );
    let mut client = connect(fake.clone(), TransportKind::Stdio).await.unwrap();
    let result = client.call_tool("tool", json!({})).await.unwrap();
    assert!(result.is_error);
    assert_eq!(result.content.len(), 5);
    assert_eq!(result.structured_content.unwrap()["error"], "safe");
    fake.done();
}

#[tokio::test]
async fn version_rejection_retry_is_finite_and_never_initializes() {
    let mut steps = vec![send(probe(1)), receive(discovered(1))];
    for id in 2..=3 {
        steps.push(send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"echo","arguments":{},"_meta":meta()}})));
        steps.push(receive(json!({"status":400,"body":{"jsonrpc":"2.0","id":id,"error":{"code":-32022,"message":"unsupported","data":{"requested":"2026-07-28","supported":["2026-07-28"]}}}})));
    }
    steps.push(golden::Step::Close);
    let fake = Fake::script(steps, TransportKind::Stdio);
    let mut client = connect(fake.clone(), TransportKind::Http).await.unwrap();
    assert_eq!(
        client.call_tool("echo", json!({})).await.unwrap_err().kind,
        McpErrorKind::ServerRejected
    );
    assert_eq!(client.era(), ProtocolEra::Modern);
    fake.done();
}
#[tokio::test]
async fn auth_error_never_initializes_even_with_modern_error_body() {
    let fake = Fake::script(
        vec![
            send(probe(1)),
            receive(
                json!({"status":401,"body":{"jsonrpc":"2.0","id":1,"error":{"code":-32022,"message":"secret","data":{"requested":"2026-07-28","supported":["2026-07-28"]}}}}),
            ),
            golden::Step::Close,
        ],
        TransportKind::Stdio,
    );
    assert_eq!(
        connect(fake.clone(), TransportKind::Http)
            .await
            .err()
            .unwrap()
            .kind,
        McpErrorKind::HttpStatus(401)
    );
    fake.done();
}

#[test]
fn header_integer_encoding_respects_mathematical_integer_and_safe_bounds() {
    let mapping = headers::parameter_headers(&tool(
        json!({"type":"object","properties":{"integer":{"type":"integer","x-mcp-header":"I"}}}),
    ))
    .unwrap();
    for (number, expected) in [
        (json!(42.0), "42"),
        (json!(-0.0), "0"),
        (json!(-9007199254740991_i64), "-9007199254740991"),
    ] {
        let body = json!({"method":"tools/call","params":{"name":"tool","arguments":{"integer":number},"_meta":meta()}});
        assert_eq!(
            headers::request_headers(&body, &mapping).unwrap()["mcp-param-i"],
            expected
        );
    }
    for number in [
        json!(0.5),
        json!(-9007199254740992_i64),
        json!("42"),
        json!(true),
    ] {
        let body = json!({"method":"tools/call","params":{"name":"tool","arguments":{"integer":number},"_meta":meta()}});
        assert!(headers::request_headers(&body, &mapping).is_err());
    }
}

#[tokio::test]
async fn legacy_server_request_refusal_preserves_negative_id() {
    let mut steps = vec![
        send(probe(1)),
        receive(json!({"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"unknown method"}})),
    ];
    steps.extend(legacy_steps());
    steps.pop();
    steps.push(send(
        json!({"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}),
    ));
    let request =
        json!({"jsonrpc":"2.0","id":-7,"method":"sampling/createMessage","params":{"messages":[]}});
    let result = json!({"jsonrpc":"2.0","id":3,"result":{"tools":[]}});
    steps.push(receive(
        json!({"chunks":[format!("data: {request}\n\ndata: {result}\n\n")]}),
    ));
    let fake = Fake::script(steps, TransportKind::Stdio);
    let mut client = connect(fake.clone(), TransportKind::Stdio).await.unwrap();
    assert!(client.list_tools().await.unwrap().is_empty());
    let responses = fake.responses.lock().unwrap();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["id"], -7);
    assert_eq!(responses[0]["error"]["code"], -32601);
    drop(responses);
    fake.done();
}
