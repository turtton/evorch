use std::sync::Arc;

use event_bus::EventBus;
use serde_json::json;
use tools::{Edit, ToolError, ToolExecutionContext, ToolExecutor};

fn executor() -> ToolExecutor {
    let mut executor = ToolExecutor::new(Arc::new(EventBus::new(16)));
    executor.register(Arc::new(Edit)).unwrap();
    executor
}

fn context() -> ToolExecutionContext {
    ToolExecutionContext {
        run_id: "r1".into(),
        thread_id: None,
        call_id: None,
    }
}

#[tokio::test]
async fn edit_accepts_str_replace_style_names() {
    // Given: a file and calls that use old_str/new_str, as str_replace-trained models do.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("note.md");
    std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
    let executor = executor();

    // When: the names appear at the top level and inside edits.
    executor
        .execute(
            &context(),
            "edit",
            "c1",
            json!({"path": path, "old_str": "one", "new_str": "ONE"}),
        )
        .await
        .unwrap();
    executor
        .execute(
            &context(),
            "edit",
            "c2",
            json!({"path": path, "edits": [
                {"old_str": "two", "new_string": "TWO"},
                {"old_string": "three", "new_str": "THREE"},
            ]}),
        )
        .await
        .unwrap();

    // Then: they are read as old_string/new_string.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "ONE\nTWO\nTHREE\n");
}

#[tokio::test]
async fn edit_rejects_an_alias_next_to_its_canonical_name() {
    // Given: conflicting canonical and alias values.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("note.md");
    std::fs::write(&path, "one\n").unwrap();

    // When: both names are supplied.
    let error = executor()
        .execute(
            &context(),
            "edit",
            "c1",
            json!({"path": path, "old_string": "one", "old_str": "one", "new_string": "ONE"}),
        )
        .await
        .unwrap_err();

    // Then: nothing is guessed and the file is unchanged.
    assert!(matches!(error, ToolError::InvalidArgs { .. }), "{error:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\n");
}
