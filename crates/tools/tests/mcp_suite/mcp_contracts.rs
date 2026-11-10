use config::mcp::{SourceRevision, Template, TemplatePart};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use tools::mcp::{McpErrorKind, McpToolDefinition, transport::*, types::*};
#[path = "../../shared-test-support/mcp/mod.rs"]
mod fixture;
fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("fixtures/mcp")
        .join(name)
}
#[derive(Deserialize)]
struct CancellationCase {
    name: String,
    era: ProtocolEra,
    transport: TransportKind,
    phase: RequestPhase,
    issued: bool,
    in_flight: bool,
    expected: CancellationAction,
}
#[test]
fn mcp_cancellation_policy_matches_golden() {
    let cases: Vec<CancellationCase> =
        serde_json::from_str(&std::fs::read_to_string(fixture_path("cancellation.json")).unwrap())
            .unwrap();
    for case in cases {
        let context = CancelRequest {
            id: RpcId::Number(7),
            era: case.era,
            transport: case.transport,
            phase: case.phase,
            issued: case.issued,
            in_flight: case.in_flight,
        };
        assert_eq!(
            cancellation_action(&context),
            case.expected,
            "{}",
            case.name
        );
    }
}
#[test]
fn mcp_fingerprints_separate_source_approval_and_definition() {
    let absent = SourceRevision::from_sources(None, None);
    assert_ne!(absent, SourceRevision::from_sources(Some(b""), None));
    assert_ne!(
        SourceRevision::from_sources(Some(b"ab"), Some(b"c")),
        SourceRevision::from_sources(Some(b"a"), Some(b"bc"))
    );
    let mut definition = McpToolDefinition {
        name: "echo".into(),
        description: Some("Echo".into()),
        input_schema: serde_json::from_value(json!({"type":"object"})).unwrap(),
    };
    let old = DefinitionFingerprint::for_definition(&definition);
    definition.description = Some("Different".into());
    assert_ne!(old, DefinitionFingerprint::for_definition(&definition));
    let canonical = serde_json::to_vec(&definition).unwrap();
    assert_ne!(
        ApprovalFingerprint::from_projection(&canonical).0,
        DefinitionFingerprint::for_definition(&definition).0
    );
    assert_ne!(
        SourceRevision::from_sources(Some(&canonical), None).0,
        ApprovalFingerprint::from_projection(&canonical).0
    );
}
#[test]
fn mcp_envelope_retains_400_and_bounds_data() {
    let error = RpcFrame::Error {
        jsonrpc: "2.0".into(),
        id: Some(RpcId::Number(1)),
        error: RpcError {
            code: -32022,
            message: "private-response-sentinel".into(),
            data: Some(json!({"supported":["2026-07-28"],"requested":"1900-01-01"})),
        },
    };
    let envelope = McpResponseEnvelope::new(
        Some(400),
        McpResponseHeaders::default(),
        vec![error.clone()],
        MAX_REQUEST_BYTES,
    )
    .unwrap();
    assert_eq!(envelope.status, Some(400));
    assert!(matches!(&envelope.frames()[0], RpcFrame::Error { error, .. } if error.code == -32022));
    assert!(!format!("{envelope:?}{error:?}").contains("private-response-sentinel"));
    assert_eq!(
        McpResponseEnvelope::new(
            Some(400),
            McpResponseHeaders::default(),
            vec![],
            MAX_REQUEST_BYTES + 1
        )
        .unwrap_err(),
        McpErrorKind::LimitExceeded
    );
    let huge = RpcFrame::Result {
        jsonrpc: "2.0".into(),
        id: RpcId::Number(1),
        result: Value::String("x".repeat(MAX_FRAME_BYTES)),
    };
    assert_eq!(
        McpResponseEnvelope::new(None, McpResponseHeaders::default(), vec![huge], 0).unwrap_err(),
        McpErrorKind::LimitExceeded
    );
}
#[test]
fn mcp_configuration_and_rpc_debug_are_redacted() {
    let template = Template(vec![TemplatePart::Literal {
        value: "config-secret-sentinel".into(),
    }]);
    assert!(!format!("{template:?}{:?}", template.0[0]).contains("config-secret-sentinel"));
    let request = RpcRequest {
        jsonrpc: "2.0".into(),
        id: RpcId::Number(1),
        method: "tools/call".into(),
        params: json!({"token":"rpc-secret-sentinel"}),
    };
    assert!(!format!("{request:?}").contains("rpc-secret-sentinel"));
}
#[tokio::test]
async fn mcp_cancellation_signal_is_sticky() {
    let context = CancelContext::default();
    context.cancel();
    context.cancelled().await;
    context.clone().cancelled().await;
    assert!(context.is_cancelled());
}
#[tokio::test]
async fn mcp_fixture_gate_requires_explicit_event() {
    let steps = fixture::load(&fixture_path("modern-http.json"));
    let mut driver = fixture::spawn(steps.clone());
    for step in steps {
        match step {
            fixture::Step::Send { value } => driver.input.send(value).await.unwrap(),
            fixture::Step::Receive { value } => assert_eq!(
                driver.output.recv().await,
                Some(fixture::PeerEvent::Received(value))
            ),
            fixture::Step::Signal { name } => assert_eq!(
                driver.output.recv().await,
                Some(fixture::PeerEvent::Signal(name))
            ),
            fixture::Step::Gate { name } => {
                assert!(!driver.done.is_finished());
                driver.control.send(name).await.unwrap();
            }
            fixture::Step::Close => {
                assert_eq!(driver.output.recv().await, Some(fixture::PeerEvent::Closed))
            }
        }
    }
    assert!(driver.done.await.unwrap().is_ok());
}
#[tokio::test]
async fn mcp_fixture_driver_rejects_wrong_input() {
    let driver = fixture::spawn(vec![
        fixture::Step::Send {
            value: json!({"id":1}),
        },
        fixture::Step::Close,
    ]);
    driver.input.send(json!({"id":2})).await.unwrap();
    assert!(driver.done.await.unwrap().is_err());
}
#[test]
fn mcp_all_transcripts_parse_without_product_builders() {
    for name in [
        "modern-stdio.jsonl",
        "legacy-stdio.jsonl",
        "modern-http.json",
        "legacy-http.json",
    ] {
        let steps = fixture::load(&fixture_path(name));
        assert!(matches!(steps.last(), Some(fixture::Step::Close)));
    }
}

// This exercises the fixture process itself; C/D's golden_* tests compare product
// negotiation/transports against these same independent transcripts later.
#[tokio::test]
async fn mcp_fixture_stdio_roundtrip() {
    use std::process::Stdio;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        process::Command,
    };
    for name in ["modern-stdio.jsonl", "legacy-stdio.jsonl"] {
        let path = fixture_path(name);
        let mut child = Command::new(std::env::var("CARGO_BIN_EXE_mcp-test-server").unwrap())
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
        for step in fixture::load(&path) {
            match step {
                fixture::Step::Send { value } => {
                    let mut bytes = serde_json::to_vec(&value).unwrap();
                    bytes.push(b'\n');
                    input.write_all(&bytes).await.unwrap();
                    input.flush().await.unwrap();
                }
                fixture::Step::Receive { value } => {
                    let line = output.next_line().await.unwrap().expect("fixture response");
                    assert_eq!(serde_json::from_str::<Value>(&line).unwrap(), value);
                }
                fixture::Step::Close => (),
                _ => panic!("stdio fixture does not use in-memory gates"),
            }
        }
        drop(input);
        assert!(child.wait().await.unwrap().success());
    }
}

#[test]
fn mcp_canonical_json_sorts_nested_objects_but_preserves_array_order() {
    let left: Value = serde_json::from_str(r#"{"z":{"b":2,"a":1},"a":[2,1]}"#).unwrap();
    let right: Value = serde_json::from_str(r#"{"a":[2,1],"z":{"a":1,"b":2}}"#).unwrap();
    assert_eq!(canonical_json_bytes(&left), canonical_json_bytes(&right));
    assert_ne!(
        canonical_json_bytes(&left),
        canonical_json_bytes(&json!({"a":[1,2],"z":{"a":1,"b":2}}))
    );
}
