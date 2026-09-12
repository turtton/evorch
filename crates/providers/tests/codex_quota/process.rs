use super::*;

#[cfg(target_os = "linux")]
struct PidFile(std::path::PathBuf);

#[cfg(target_os = "linux")]
impl PidFile {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        Self(std::env::temp_dir().join(format!(
            "evorch-quota-{}-{}.pid",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )))
    }
}

#[cfg(target_os = "linux")]
impl Drop for PidFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[tokio::test]
async fn unrelated_frames_are_capped_before_timeout() {
    // Given: more unrelated frames than allowed, followed by a hung server.
    let config = QuotaConfig {
        app_server_program: "sh".into(),
        app_server_args: vec!["-c".into(),
            "i=0; while [ $i -lt 129 ]; do printf '%s\\n' '{\"id\":999,\"result\":{}}'; i=$((i+1)); done; IFS= read -r r; IFS= read -r r".into()],
        timeout: Duration::from_millis(500),
        ..QuotaConfig::default()
    };
    let mut client = CodexQuotaClient::new(config, Arc::new(InMemoryTokenStore::new())).unwrap();
    // When
    let error = client.fetch_quota().await.unwrap_err();
    // Then
    let QuotaError::Sources { app_server, .. } = error else {
        panic!("expected sources")
    };
    assert!(
        matches!(
            *app_server,
            QuotaError::Protocol("too many unrelated RPC frames")
        ),
        "{app_server}"
    );
}

#[cfg(target_os = "linux")]
fn hanging_config(pid: &std::path::Path) -> QuotaConfig {
    QuotaConfig {
        app_server_program: "sh".into(),
        app_server_args: vec![
            "-c".into(),
            "printf '%s' $$ > \"$1\"; while :; do :; done".into(),
            "fixture".into(),
            pid.to_str().unwrap().into(),
        ],
        timeout: Duration::from_millis(500),
        ..QuotaConfig::default()
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn timeout_reaps_child_before_returning() {
    // Given: a real child that cannot exit merely because stdin closes.
    let file = PidFile::new();
    let pid = &file.0;
    let mut client =
        CodexQuotaClient::new(hanging_config(pid), Arc::new(InMemoryTokenStore::new())).unwrap();
    // When
    let error = client.fetch_quota().await.unwrap_err();
    // Then: even a zombie would still have a proc entry here.
    let QuotaError::Sources { app_server, .. } = error else {
        panic!("expected sources")
    };
    assert!(matches!(*app_server, QuotaError::Timeout));
    let pid = std::fs::read_to_string(pid).unwrap();
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "child {pid} not reaped"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancelled_request_reaps_child_while_runtime_remains_active() {
    // Given: observe the child starting before cancelling the request.
    let file = PidFile::new();
    let pid_path = &file.0;
    let mut config = hanging_config(pid_path);
    config.timeout = Duration::from_secs(60);
    let mut client = CodexQuotaClient::new(config, Arc::new(InMemoryTokenStore::new())).unwrap();
    let task = tokio::spawn(async move { client.fetch_quota().await });
    let pid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(pid) = std::fs::read_to_string(pid_path)
                && !pid.is_empty()
            {
                break pid;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // When
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    // Then: asynchronous cancellation cleanup must complete without the RPC deadline.
    tokio::time::timeout(Duration::from_secs(5), async {
        while std::path::Path::new(&format!("/proc/{pid}")).exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled child reaped");
}
