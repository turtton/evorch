//! Regression coverage for run-61: excessive output, partial timeouts and cwd.
use event_bus::{EventBus, EventKind, ToolEvent};
use sandbox::DirectSandbox;
use serde_json::json;
use std::sync::Arc;
use tools::{Shell, Tool, ToolExecutionContext, ToolExecutor};

#[tokio::test]
async fn large_shell_output_is_bounded_redacted_and_recoverable() {
    let secret = format!("sk-{}", "a".repeat(48));
    let result = Shell::new(Arc::new(DirectSandbox::new_unchecked())).execute(json!({
        "command": format!("i=0; while [ $i -lt 4000 ]; do printf 'line %s abcdefghijklmnopqrstuvwxyz\\n' \"$i\"; i=$((i+1)); done; printf '%s\\n' '{secret}'"),
    })).await.unwrap();
    assert!(!result.is_error);
    assert!(result.content.len() < 20 * 1024);
    assert!(!result.content.contains(&secret));
    let path = result.detail.as_ref().unwrap()["output_artifact"]["path"]
        .as_str()
        .unwrap();
    let artifact = std::fs::read_to_string(path).unwrap();
    assert!(artifact.contains("line 0 abcdef"));
    assert!(artifact.contains("line 3999 abcdef"));
    assert!(!artifact.contains(&secret));
}

#[tokio::test]
async fn timeout_keeps_partial_output_even_when_descendant_holds_pipe() {
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        Shell::new(Arc::new(DirectSandbox::new_unchecked())).execute(json!({
            "command": "printf 'before timeout\\n'; sleep 30 & wait", "timeout_ms": 100
        })),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(result.is_error);
    assert!(result.content.contains("before timeout"));
    assert!(result.content.contains("timed out after 100 ms"));
}

#[tokio::test]
async fn relative_file_paths_and_explicit_dot_share_workspace() {
    let root = tempfile::tempdir().unwrap();
    let bus = Arc::new(EventBus::new(32));
    let mut events = bus.subscribe();
    let executor = ToolExecutor::with_standard_tools_in(
        bus,
        Arc::new(DirectSandbox::new_unchecked()),
        Some(root.path().to_path_buf()),
    );
    let ctx = ToolExecutionContext {
        run_id: "run-test".into(),
        thread_id: None,
        call_id: None,
    };
    executor
        .execute(
            &ctx,
            "write",
            "write-1",
            json!({"path":"created.txt", "content":"hello"}),
        )
        .await
        .unwrap();
    let read = executor
        .execute(&ctx, "read", "read-1", json!({"path":"created.txt"}))
        .await
        .unwrap();
    assert!(read.content.contains("hello"));
    let shell = executor
        .execute(
            &ctx,
            "shell",
            "shell-1",
            json!({"command":"pwd; cat created.txt", "cwd":"."}),
        )
        .await
        .unwrap();
    assert!(shell.content.contains(root.path().to_str().unwrap()));
    assert!(shell.content.ends_with("hello"));
    for _ in 0..6 {
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();
        if let EventKind::Tool(ToolEvent::ToolCompleted {
            output: Some(output),
            ..
        }) = event.kind
        {
            assert!(output.len() < 20 * 1024);
        }
    }
}

#[tokio::test]
async fn large_write_diff_is_bounded_before_reaching_the_agent_and_gui() {
    let root = tempfile::tempdir().unwrap();
    let bus = Arc::new(EventBus::new(16));
    let mut events = bus.subscribe();
    let executor = ToolExecutor::with_standard_tools_in(
        bus,
        Arc::new(DirectSandbox::new_unchecked()),
        Some(root.path().to_path_buf()),
    );
    let content = "new line with content\n".repeat(1000);
    let result = executor
        .execute(
            &ToolExecutionContext {
                run_id: "run-diff".into(),
                thread_id: None,
                call_id: None,
            },
            "write",
            "write-diff",
            json!({"path": "large.txt", "content": content}),
        )
        .await
        .unwrap();

    assert!(result.content.len() < 20 * 1024);
    assert!(result.content.contains("[Output artifact:"));
    assert!(result.detail.as_ref().unwrap()["output_artifact"]["path"].is_string());
    events.recv().await.unwrap();
    let completed = events.recv().await.unwrap();
    let EventKind::Tool(ToolEvent::ToolCompleted { output, .. }) = completed.kind else {
        panic!("expected completed tool event");
    };
    assert_eq!(output.as_deref(), Some(result.content.as_str()));
}
