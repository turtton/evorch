//! Test-only helpers; I0 owns product verification and the manifest contract.
use std::{
    io::{BufRead, BufReader, Read},
    path::Path,
    process::{Command, Stdio},
};
pub struct Child(pub std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
pub fn command(root: &Path, sockets: &Path) -> Command {
    let artifact = test_support::product_artifact::require_from_env();
    let mut command = Command::new(artifact.exe);
    command
        .arg("--config-dir")
        .arg(root.join("config"))
        .env("XDG_STATE_HOME", sockets)
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", sockets)
        .env("TOKIO_WORKER_THREADS", "2")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}
pub fn ready(child: &mut Child) -> (BufReader<std::process::ChildStdout>, serde_json::Value) {
    let mut out = BufReader::new(child.0.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        if out.read_line(&mut line).unwrap() == 0 {
            let mut error = String::new();
            child
                .0
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut error)
                .unwrap();
            panic!("product ended before Ready: {error}");
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line)
            && value["status"] == "ready"
        {
            return (out, value);
        }
    }
}
pub fn hello(socket: &Path, epoch: Option<u64>, identity: &str) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let client = workbench_client::WorkbenchClient::connect(
                workbench_client::UnixTransport::connect(socket)
                    .await
                    .unwrap(),
                workbench_client::ClientConfig::new(
                    workbench_protocol::ids::ClientId::new("watchdog").unwrap(),
                    workbench_protocol::handshake::BuildInfo {
                        name: "watchdog-test".into(),
                        version: "1".into(),
                        revision: None,
                    },
                ),
            )
            .await
            .unwrap();
            if let Some(epoch) = epoch {
                assert_eq!(client.server_hello().instance_epoch.0, epoch);
            }
            assert_eq!(client.server_hello().core.as_str(), identity);
        });
}
pub fn term(child: &Child) {
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(child.0.id() as i32).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
}
pub fn wait(child: &mut Child, code: i32) -> String {
    let status = child.0.wait().unwrap();
    let mut error = String::new();
    child
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut error)
        .unwrap();
    assert_eq!(status.code(), Some(code), "{error}");
    assert!(!error.contains("planned shutdown exceeded 10s"), "{error}");
    assert!(!error.contains("watchdog wiring incomplete"), "{error}");
    error
}
