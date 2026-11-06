//! Process readiness is a line emitted after bind, not a wall-clock guess.
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use workbench_client::{ClientConfig, UnixTransport, WorkbenchClient};
use workbench_protocol::{handshake::*, ids::*, queries::*};

#[tokio::test]
async fn foreground_web_health_handshake_rejection_and_sigterm_release_the_lock() {
    let dir = tempfile::Builder::new()
        .prefix("p7-")
        .tempdir_in("/tmp")
        .unwrap();
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
    let http = reqwest::Client::new();
    let health = http
        .get(format!(
            "http://{}/api/v1/health",
            ready["web"].as_str().unwrap()
        ))
        .send()
        .await
        .unwrap();
    assert!(health.status().is_success());
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
        ClientConfig::new(ClientId::new("test").unwrap(), build),
    )
    .await
    .unwrap();
    assert!(matches!(
        client
            .query(Query::ListProjects(Default::default()))
            .await
            .unwrap(),
        QueryReply::Projects(_)
    ));
    let duplicate = tokio::process::Command::new(&executable)
        .args(["--config-dir", dir.path().to_str().unwrap()])
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
    // The process used /tmp, not the test's IPC parent: probe exactly that lock.
    let released =
        evorch_core::instance::Instance::acquire(dir.path(), std::path::Path::new("/tmp")).unwrap();
    drop(released);
}
