use std::sync::Arc;

use event_bus::{EventBus, EventKind, ToolEvent};
use serde_json::json;
use tools::{Read, Tool, ToolError, ToolExecutionContext, ToolExecutor};

fn executor() -> (ToolExecutor, event_bus::EventReceiver) {
    let bus = Arc::new(EventBus::new(16));
    let receiver = bus.subscribe();
    let mut executor = ToolExecutor::new(bus);
    executor.register(Arc::new(Read)).unwrap();
    (executor, receiver)
}

fn context() -> ToolExecutionContext {
    ToolExecutionContext {
        run_id: "r1".into(),
        thread_id: None,
        call_id: None,
        ..Default::default()
    }
}

macro_rules! alias_test {
    ($name:ident, $key:literal) => {
        #[tokio::test]
        async fn $name() {
            // Given: a real file and an executor that validates arguments.
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("sample.txt");
            std::fs::write(&path, "alias content\n").unwrap();
            let (executor, mut receiver) = executor();
            let input = json!({$key: path});

            // When: the model supplies a common alternative key.
            let result = executor.execute(&context(), "read", "c1", input.clone()).await.unwrap();

            // Then: the file is returned and the recorded input names it as `path`.
            assert_eq!(result.content, "alias content\n");
            let normalized = json!({"path": path});
            assert!(matches!(receiver.recv().await.unwrap().kind,
                EventKind::Tool(ToolEvent::ToolStarted { input: Some(recorded), .. }) if recorded == normalized));
        }
    };
}

alias_test!(read_accepts_file_alias, "file");
alias_test!(read_accepts_file_path_alias, "file_path");
alias_test!(read_accepts_filename_alias, "filename");
alias_test!(read_accepts_target_alias, "target");

#[tokio::test]
async fn read_rejects_aliases_with_different_values() {
    // Given: the canonical path and an alias naming different places.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("canonical.txt");
    std::fs::write(&path, "canonical").unwrap();
    let (executor, mut receiver) = executor();
    let input = json!({"path": path, "file": dir.path()});
    // When: both keys are supplied.
    let error = executor
        .execute(&context(), "read", "c1", input.clone())
        .await
        .unwrap_err();
    // Then: nothing is read, and the message names each key with its value.
    let ToolError::InvalidArgs { detail } = &error else {
        panic!("expected InvalidArgs, got {error:?}");
    };
    assert!(
        detail.contains(&format!("path=\"{}\"", path.display())),
        "{detail}"
    );
    assert!(
        detail.contains(&format!("file=\"{}\"", dir.path().display())),
        "{detail}"
    );
    assert!(detail.contains("path 引数 1 つ"), "{detail}");
    assert!(matches!(receiver.recv().await.unwrap().kind,
        EventKind::Tool(ToolEvent::ToolStarted { input: Some(recorded), .. }) if recorded == input));
    assert!(matches!(
        receiver.recv().await.unwrap().kind,
        EventKind::Tool(ToolEvent::ToolCompleted { is_error: true, .. })
    ));
    assert!(matches!(
        Read.execute(input).await,
        Err(ToolError::InvalidArgs { .. })
    ));
}

#[tokio::test]
async fn read_accepts_equal_duplicates_and_empty_aliases() {
    // Given: a file named by several aliases with the same value, plus empty ones.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("same.txt");
    std::fs::write(&path, "same").unwrap();
    let (executor, _) = executor();
    // When: the call is executed.
    let result = executor
        .execute(
            &context(),
            "read",
            "c1",
            json!({"path": path, "file": path, "file_path": path, "filename": "", "target": ""}),
        )
        .await
        .unwrap();
    // Then: the duplicates collapse into one path.
    assert_eq!(result.content, "same");
}

#[test]
fn read_schema_lists_only_path_for_the_file() {
    // Given / When: the model-facing read schema.
    let schema = Read.schema();
    // Then: aliases are accepted at run time but not advertised.
    let properties = schema["properties"].as_object().unwrap();
    assert!(properties.contains_key("path"));
    for alias in ["file", "file_path", "filename", "target"] {
        assert!(!properties.contains_key(alias), "{alias}");
    }
}

#[tokio::test]
async fn read_error_message_mentions_expected_keys() {
    // Given: a registered read tool.
    let (executor, _) = executor();
    // When: no path key is supplied.
    let error = executor
        .execute(&context(), "read", "c1", json!({}))
        .await
        .unwrap_err();
    // Then: the model receives an actionable argument hint.
    assert!(matches!(error, ToolError::InvalidArgs { .. }));
    assert!(error.to_string().contains("引数 path は文字列"));
}

#[tokio::test]
async fn read_direct_call_accepts_alias() {
    // Given: a file and a direct tool invocation.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("direct.txt");
    std::fs::write(&path, "direct").unwrap();
    // When: the executor is bypassed.
    let result = Read.execute(json!({"file": path})).await.unwrap();
    // Then: alias handling is also available at the tool boundary.
    assert_eq!(result.content, "direct");
}

#[tokio::test]
async fn tool_completed_error_carries_output() {
    // Given: a schema-invalid read call and a subscribed event bus.
    let (executor, mut receiver) = executor();
    // When: validation rejects the input.
    let error = executor
        .execute(&context(), "read", "c1", json!({"path": 7}))
        .await
        .unwrap_err();
    // Then: the completion payload preserves the same error returned to the caller.
    assert!(matches!(error, ToolError::InvalidArgs { .. }));
    receiver.recv().await.unwrap();
    let event = receiver.recv().await.unwrap();
    let serialized = serde_json::to_value(&event).unwrap();
    assert_eq!(
        serialized["kind"]["payload"]["payload"]["output"],
        error.to_string()
    );
    assert!(matches!(
        event.kind,
        EventKind::Tool(ToolEvent::ToolCompleted { is_error: true, .. })
    ));
}

#[tokio::test]
async fn tool_execution_error_carries_output() {
    // Given: a valid path pointing to a directory.
    let dir = tempfile::tempdir().unwrap();
    let (executor, mut receiver) = executor();
    // When: the tool itself rejects the target.
    let error = executor
        .execute(&context(), "read", "c1", json!({"path": dir.path()}))
        .await
        .unwrap_err();
    // Then: execution errors, not only schema errors, retain their message.
    assert!(matches!(error, ToolError::NotAFile { .. }));
    receiver.recv().await.unwrap();
    assert!(matches!(receiver.recv().await.unwrap().kind,
        EventKind::Tool(ToolEvent::ToolCompleted { is_error: true, output: Some(output), .. })
        if output == error.to_string()));
}
