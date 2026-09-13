use std::{sync::Arc, time::Duration};

use event_bus::{EventBus, EventKind};
use sandbox::{CommandSpec, DirectSandbox};
use tools::{Tool, ToolExecutionContext, lsp::LspDiagnostics};

fn fixture(mode: &str) -> (tempfile::TempDir, LspDiagnostics, Arc<EventBus>) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sample file.rs"), "secret document").unwrap();
    let bus = Arc::new(EventBus::new(16));
    let command = CommandSpec {
        program: env!("CARGO_BIN_EXE_lsp-test-server").into(),
        args: vec![mode.into(), dir.path().join("pid").display().to_string()],
        cwd: None,
        extra_env: vec![],
    };
    let tool = LspDiagnostics::new(
        Arc::new(DirectSandbox::new_unchecked()),
        command,
        "rust".into(),
    )
    .with_timeout(Duration::from_secs(2))
    .with_event_bus(bus.clone());
    (dir, tool, bus)
}

#[tokio::test]
async fn renders_canonical_lines_when_server_publishes() {
    // Given: a real framed stdio server and an owning run/thread.
    let (dir, tool, bus) = fixture("ok");
    let mut events = bus.subscribe();
    let path = dir.path().join("sample file.rs");
    // When: opening a document through the Tool surface.
    let result = tool
        .execute_with_context(
            &ToolExecutionContext {
                run_id: "run-lsp".into(),
                thread_id: Some("thread-lsp".into()),
                call_id: Some("call-lsp".into()),
            },
            serde_json::json!({"path":path}),
        )
        .await
        .unwrap();
    // Then: every LSP severity is rendered in agent-visible content.
    let expected = ["error", "warning", "information", "hint"]
        .iter()
        .enumerate()
        .map(|(index, label)| {
            format!(
                "{}:2:3 {label} [{}] message {}",
                path.display(),
                index + 1,
                index + 1
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, expected);
    let EventKind::Diagnostic(event) = tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap()
        .kind
    else {
        panic!("diagnostic")
    };
    assert_eq!(event.run_id.as_deref(), Some("run-lsp"));
    assert_eq!(event.thread_id.as_deref(), Some("thread-lsp"));
    assert_eq!(event.call_id.as_deref(), Some("call-lsp"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&event.detail).unwrap(),
        serde_json::json!({"file":path,"count":4,"codes":["1","2","3","4"]})
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(10), events.recv())
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("pid.exit")).unwrap(),
        "graceful"
    );
}

#[tokio::test]
async fn returns_error_and_reaps_when_server_crashes_or_stalls() {
    for mode in ["crash", "hang", "malformed"] {
        // Given: a server that fails after didOpen.
        let (dir, tool, _) = fixture(mode);
        // When: executing with a bounded outer deadline too.
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            tool.execute(serde_json::json!({"path":dir.path().join("sample file.rs")})),
        )
        .await
        .unwrap()
        .unwrap();
        // Then: no panic or zombie is left, and failure reaches the agent.
        assert!(result.is_error, "{}", result.content);
        let pid = std::fs::read_to_string(dir.path().join("pid")).unwrap();
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }
}

#[tokio::test]
async fn returns_empty_success_when_server_clears_diagnostics() {
    // Given: a server publishing an empty batch.
    let (dir, tool, _) = fixture("empty");
    // When: requesting diagnostics.
    let result = tool
        .execute(serde_json::json!({"path":dir.path().join("sample file.rs")}))
        .await
        .unwrap();
    // Then: clearing is a successful empty result, not a timeout.
    assert!(!result.is_error);
    assert_eq!(result.content, "");
    assert_eq!(result.detail.unwrap()["count"], 0);
}
