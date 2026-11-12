//! The real core process and the typed namespaces: a protocol version 1
//! client is refused at the handshake, only implemented capabilities are
//! offered, and nothing outside the namespace table gets through.

use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use workbench_protocol::{
    ErrorCode,
    envelope::{ReplyResult, ServerFrame},
    handshake::{
        BuildInfo, Capability, ClientHello, HandshakeReply, ProtocolVersion, RecommendedAction,
        RejectReason,
    },
    ids::{CapabilityName, ClientId},
};

async fn exchange(stream: &mut UnixStream, value: &impl serde::Serialize) -> Value {
    let bytes = workbench_protocol::framing::encode_frame(value, 1024 * 1024).unwrap();
    stream.write_all(&bytes).await.unwrap();
    let len = stream.read_u32().await.unwrap();
    assert!(len <= 1024 * 1024);
    let mut bytes = vec![0; len as usize];
    stream.read_exact(&mut bytes).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn hello(versions: &[ProtocolVersion], required: &[&str]) -> ClientHello {
    ClientHello {
        supported_versions: versions.to_vec(),
        required_capabilities: required
            .iter()
            .map(|name| CapabilityName::new(*name).unwrap())
            .collect(),
        optional_capabilities: Vec::new(),
        client_build: BuildInfo {
            name: "test".into(),
            version: "1".into(),
            revision: None,
        },
        client_instance_id: ClientId::new("namespaces").unwrap(),
        desired_core: None,
    }
}

async fn connect(socket: &str, hello: &ClientHello) -> (UnixStream, HandshakeReply) {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    let reply = exchange(&mut stream, hello).await;
    (stream, serde_json::from_value(reply).unwrap())
}

fn request(kind: &str, payload: Value) -> Value {
    json!({"request_id": "req-ns", "session_id": "sess-ns", "kind": kind, "payload": payload})
}

async fn error_code(stream: &mut UnixStream, request: Value) -> ErrorCode {
    let ServerFrame::Reply(reply) =
        serde_json::from_value(exchange(stream, &request).await).unwrap()
    else {
        panic!("expected a reply");
    };
    let ReplyResult::Error(error) = reply.result else {
        panic!("the request was accepted: {request}");
    };
    error.code
}

#[tokio::test]
async fn a_version_1_client_is_refused_and_only_implemented_capabilities_are_offered() {
    let dir = tempfile::Builder::new()
        .prefix("p8-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "[web]\nport = 0\n[core]\nidle_grace_secs = 0\n",
    )
    .unwrap();
    let executable = std::env::var_os("CARGO_BIN_EXE_evorch-core").unwrap();
    let mut child = tokio::process::Command::new(&executable)
        .args([
            "--config-dir",
            dir.path().to_str().unwrap(),
            "--foreground",
            "--web",
        ])
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .env("XDG_RUNTIME_DIR", dir.path())
        .current_dir(dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let ready: Value =
        serde_json::from_str(&lines.next_line().await.unwrap().expect("readiness line")).unwrap();
    let socket = ready["socket"].as_str().unwrap().to_owned();
    let root = format!("http://{}", ready["web"].as_str().unwrap());

    // A version 1 client is told so; the core does not speak down to it.
    let (_, reply) = connect(&socket, &hello(&[ProtocolVersion(1)], &[])).await;
    let HandshakeReply::Rejected(rejected) = reply else {
        panic!("a version 1 client was accepted");
    };
    assert_eq!(rejected.reason, RejectReason::UnsupportedVersion);
    assert_eq!(
        rejected.recommended_action,
        RecommendedAction::UpgradeClient
    );
    assert_eq!(rejected.supported_versions, [ProtocolVersion::CURRENT]);

    // What the core offers is the explicit implemented set; safe restart is not in it.
    let (mut stream, reply) = connect(
        &socket,
        &hello(&[ProtocolVersion::CURRENT], &["queries.v1"]),
    )
    .await;
    let HandshakeReply::Accepted(server) = reply else {
        panic!("the current version was refused");
    };
    assert_eq!(server.protocol_version, ProtocolVersion::CURRENT);
    assert_eq!(server.capabilities, [Capability::Queries]);
    let (_, reply) = connect(
        &socket,
        &hello(
            &[ProtocolVersion::CURRENT],
            &[Capability::SafeRestart.name()],
        ),
    )
    .await;
    let HandshakeReply::Rejected(rejected) = reply else {
        panic!("an unadvertised capability was granted");
    };
    assert_eq!(rejected.reason, RejectReason::MissingCapabilities);

    // A capability that was not negotiated refuses the request before it runs.
    let command = request(
        "command",
        json!({"type": "execution", "payload": {"type": "stop_chat", "payload": {"thread": "t"}}}),
    );
    assert_eq!(
        error_code(&mut stream, command).await,
        ErrorCode::UnsupportedCapability
    );

    // Reserved namespaces and the flat version 1 shape are unsupported, not accepted.
    for payload in [
        json!({"type": "settings", "payload": {"type": "write", "payload": {}}}),
        json!({"type": "maintenance", "payload": {"type": "restart"}}),
        json!({"type": "get_core_status"}),
    ] {
        assert_eq!(
            error_code(&mut stream, request("query", payload.clone())).await,
            ErrorCode::UnsupportedCapability,
            "{payload}"
        );
    }
    let http = reqwest::Client::new();
    let response = http
        .post(format!("{root}/api/v1/request"))
        .header("Origin", &root)
        .json(&request(
            "command",
            json!({"type": "auth", "payload": {"type": "login", "payload": {}}}),
        ))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_client_error());
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["payload"]["code"], "unsupported_capability");

    rustix::process::kill_process(
        rustix::process::Pid::from_raw(child.id().unwrap() as i32).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    assert!(child.wait().await.unwrap().success());
}
