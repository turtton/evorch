//! Process readiness is a line emitted after bind, not a wall-clock guess.
use std::{os::unix::fs::PermissionsExt, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use workbench_client::{ClientConfig, UnixTransport, WorkbenchClient};
use workbench_protocol::namespaces::{
    LifecycleQuery, LifecycleReply, WorkspaceQuery, WorkspaceReply,
};
use workbench_protocol::{envelope::*, errors::ErrorCode, handshake::*, ids::*, queries::*};

async fn exchange(stream: &mut UnixStream, value: &impl serde::Serialize) -> serde_json::Value {
    let bytes = workbench_protocol::framing::encode_frame(value, 1024 * 1024).unwrap();
    stream.write_all(&bytes).await.unwrap();
    let len = stream.read_u32().await.unwrap();
    assert!(len <= 1024 * 1024);
    let mut bytes = vec![0; len as usize];
    stream.read_exact(&mut bytes).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn rejects_input_reflection(
    http: &reqwest::Client,
    root: &str,
    socket: &str,
    build: BuildInfo,
) {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    let hello = exchange(
        &mut stream,
        &ClientHello {
            supported_versions: vec![ProtocolVersion::CURRENT],
            required_capabilities: vec![],
            optional_capabilities: vec![
                Capability::SnapshotEvents.into(),
                Capability::Queries.into(),
            ],
            client_build: build,
            client_instance_id: ClientId::new("raw-client").unwrap(),
            desired_core: None,
        },
    )
    .await;
    assert!(matches!(
        serde_json::from_value::<HandshakeReply>(hello).unwrap(),
        HandshakeReply::Accepted(_)
    ));
    let secret = format!("sk-proj-{}", "B".repeat(64));
    let base = serde_json::json!({
        "request_id":"bad-request", "session_id":"raw-session", "kind":"query",
        "payload":{"type":"lifecycle", "payload":{"type":"get_core_status"}}
    });
    let mut unknown_type = base.clone();
    unknown_type["payload"]["type"] = secret.clone().into();
    let mut unknown_field = base.clone();
    unknown_field[&secret] = true.into();
    let mut nested_field = serde_json::json!({
        "request_id":"bad-request", "session_id":"raw-session", "kind":"command",
        "payload":{"type":"execution", "payload":{"type":"stop_chat", "payload":{"thread":"t"}}}
    });
    nested_field["payload"]["payload"]["payload"][&secret] = true.into();
    let mut reflected_id = base.clone();
    reflected_id["request_id"] = secret.clone().into();
    reflected_id["payload"]["type"] = "unknown_query".into();
    let mut malformed = base;
    malformed["kind"] = secret.clone().into();
    for (value, code) in [
        (unknown_type, ErrorCode::UnsupportedCapability),
        (unknown_field, ErrorCode::InvalidRequest),
        (nested_field, ErrorCode::InvalidRequest),
        (malformed, ErrorCode::InvalidRequest),
        (reflected_id, ErrorCode::UnsupportedCapability),
    ] {
        let response = http
            .post(format!("{root}/api/v1/request"))
            .header("Origin", root)
            .json(&value)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_client_error());
        let body = response.text().await.unwrap();
        assert!(!body.contains("sk-proj-"), "{body}");
        let reply: ReplyEnvelope = serde_json::from_str(&body).unwrap();
        let ReplyResult::Error(error) = reply.result else {
            panic!("malformed HTTP request was accepted");
        };
        assert_eq!(error.code, code);
        let body = exchange(&mut stream, &value).await;
        assert!(!body.to_string().contains("sk-proj-"), "{body}");
        let ServerFrame::Reply(reply) = serde_json::from_value(body).unwrap() else {
            panic!("expected Unix rejection reply");
        };
        let ReplyResult::Error(error) = reply.result else {
            panic!("malformed Unix request was accepted");
        };
        assert_eq!(error.code, code);
    }
    for (kind, payload) in [
        (
            "control",
            serde_json::json!({"type":"ping", "payload":{"nonce":"7"}}),
        ),
        (
            "control",
            serde_json::json!({"type":"pong", "payload":{"nonce":"7"}}),
        ),
        (
            "query",
            serde_json::json!({"type":"lifecycle", "payload":{"type":"get_core_status"}}),
        ),
    ] {
        let value = serde_json::json!({"request_id":secret, "session_id":"raw-session", "kind":kind, "payload":payload});
        let response = http
            .post(format!("{root}/api/v1/request"))
            .header("Origin", root)
            .json(&value)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let body = response.text().await.unwrap();
        assert!(!body.contains("sk-proj-"), "{body}");
        let reply: ReplyEnvelope = serde_json::from_str(&body).unwrap();
        assert!(matches!(reply.result, ReplyResult::Success(_)));
        let body = exchange(&mut stream, &value).await;
        assert!(!body.to_string().contains("sk-proj-"), "{body}");
        assert!(matches!(
            serde_json::from_value::<ServerFrame>(body).unwrap(),
            ServerFrame::Reply(ReplyEnvelope {
                result: ReplyResult::Success(_),
                ..
            })
        ));
    }
    // The successful Unix Subscribe takes an early reply path, while HTTP
    // synthesizes an InvalidRequest without entering the backend.
    let value = serde_json::json!({"request_id":secret, "session_id":"raw-session", "kind":"control", "payload":{"type":"subscribe", "payload":{"scope":{"type":"all"}, "after":null}}});
    let response = http
        .post(format!("{root}/api/v1/request"))
        .header("Origin", root)
        .json(&value)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_client_error());
    assert!(!response.text().await.unwrap().contains("sk-proj-"));
    let body = exchange(&mut stream, &value).await;
    assert!(!body.to_string().contains("sk-proj-"), "{body}");
    assert!(matches!(
        serde_json::from_value::<ServerFrame>(body).unwrap(),
        ServerFrame::Reply(ReplyEnvelope {
            result: ReplyResult::Success(ReplyPayload::SubscriptionStarted(_)),
            ..
        })
    ));
}

#[tokio::test]
async fn foreground_web_health_handshake_rejection_and_sigterm_release_the_lock() {
    let dir = test_socket_dir::tempdir();
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
    let ready: serde_json::Value =
        serde_json::from_str(&lines.next_line().await.unwrap().expect("readiness line")).unwrap();
    assert_eq!(ready["status"], "ready");
    let socket = ready["socket"].as_str().unwrap();
    assert!(std::path::Path::new(socket).starts_with(dir.path()));
    let http = reqwest::Client::new();
    let root = format!("http://{}", ready["web"].as_str().unwrap());
    let health = http
        .get(format!("{root}/api/v1/health"))
        .send()
        .await
        .unwrap();
    assert!(health.status().is_success());
    let health: serde_json::Value = health.json().await.unwrap();
    let status: CoreStatus = serde_json::from_value(health["hello"]["status"].clone()).unwrap();
    assert_eq!(status.running.clients, 0);
    assert!(
        status
            .retention_reasons
            .contains(&RetentionReason::WebServing)
    );
    let build = BuildInfo {
        name: "test".into(),
        version: "1".into(),
        revision: None,
    };
    let mut mismatch = ClientConfig::new(ClientId::new("mismatch").unwrap(), build.clone());
    mismatch.supported_versions = vec![ProtocolVersion(99)];
    let rejected =
        WorkbenchClient::connect(UnixTransport::connect(socket).await.unwrap(), mismatch).await;
    assert!(rejected.is_err());
    let client = WorkbenchClient::connect(
        UnixTransport::connect(socket).await.unwrap(),
        ClientConfig::new(ClientId::new("test").unwrap(), build.clone()),
    )
    .await
    .unwrap();
    assert!(matches!(
        client
            .query(Query::Workspace(WorkspaceQuery::ListProjects(
                Default::default()
            )))
            .await
            .unwrap(),
        QueryReply::Workspace(WorkspaceReply::Projects(_))
    ));
    let QueryReply::Lifecycle(LifecycleReply::CoreStatus(status)) = client
        .query(Query::Lifecycle(LifecycleQuery::GetCoreStatus))
        .await
        .unwrap()
    else {
        panic!("expected core status");
    };
    assert!(status.running.clients >= 1);
    assert!(
        status
            .retention_reasons
            .contains(&RetentionReason::ConnectedClients)
    );
    assert!(
        status
            .retention_reasons
            .contains(&RetentionReason::WebServing)
    );
    rejects_input_reflection(&http, &root, socket, build).await;
    let duplicate = tokio::process::Command::new(&executable)
        .args(["--config-dir", dir.path().to_str().unwrap()])
        .env_remove("XDG_RUNTIME_DIR")
        .current_dir(dir.path())
        .output()
        .await
        .unwrap();
    assert_eq!(duplicate.status.code(), Some(2));
    assert!(
        String::from_utf8(duplicate.stderr)
            .unwrap()
            .contains("already running")
    );
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(child.id().unwrap() as i32).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    assert!(child.wait().await.unwrap().success());
    client.closed().await;
    assert!(!std::path::Path::new(socket).exists());
    // Runtime directory selection must not change the database's lifetime lock.
    let released = evorch_core::instance::Instance::acquire(
        &evorch_core::instance::LaunchGuard::acquire(
            &evorch_core::instance::ConfigIdentity::new(dir.path()).unwrap(),
        )
        .unwrap(),
        std::path::Path::new("/tmp"),
    )
    .unwrap();
    drop(released);
}

#[test]
fn cold_spawned_real_core_adopts_launch_and_publishes_handshake_epoch() {
    use evorch_core::instance::{ConfigIdentity, LaunchGuard};
    use std::io::BufRead;
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let dir = test_socket_dir::tempdir();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "[web]\nport = 0\n[core]\nidle_grace_secs = 3600\n",
    )
    .unwrap();
    let id = ConfigIdentity::new(dir.path()).unwrap();
    let launch = LaunchGuard::acquire(&id).unwrap();
    assert!(launch.lifetime_available().unwrap());
    let mut command =
        std::process::Command::new(std::env::var_os("CARGO_BIN_EXE_evorch-core").unwrap());
    command
        .arg("--config-dir")
        .arg(dir.path())
        .arg("--web")
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .env("XDG_RUNTIME_DIR", dir.path())
        .current_dir(dir.path())
        .stdout(Stdio::piped());
    let mut child = Child(launch.spawn(command).unwrap());
    let mut line = String::new();
    std::io::BufReader::new(child.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let ready: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(ready["status"], "ready");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let record = id.read_record().unwrap().unwrap();
    let client = rt.block_on(async {
        WorkbenchClient::connect(
            UnixTransport::connect(&record.socket).await.unwrap(),
            ClientConfig::new(
                ClientId::new("cold-real-core").unwrap(),
                BuildInfo {
                    name: "test".into(),
                    version: "1".into(),
                    revision: None,
                },
            ),
        )
        .await
        .unwrap()
    });
    assert_eq!(client.server_hello().instance_epoch.0, record.epoch);
    assert_eq!(client.server_hello().core.as_str(), id.identity());
    drop(launch);
    let launch = LaunchGuard::acquire(&id).unwrap();
    assert!(!launch.lifetime_available().unwrap());
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(child.0.id() as i32).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    assert!(child.0.wait().unwrap().success());
    assert!(launch.lifetime_available().unwrap());
}
