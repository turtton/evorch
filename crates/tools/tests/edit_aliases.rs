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
        ..Default::default()
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

/// Every property of the edit schema, as models that never omit one send them.
fn all_seven_keys(path: &std::path::Path, single: serde_json::Value) -> serde_json::Value {
    let mut args = json!({
        "path": path,
        "edits": [{"old_string": "one", "new_string": "ONE", "replace_all": false}],
        "justification": "",
        "sandbox_access": "isolated",
    });
    for (key, value) in single.as_object().unwrap() {
        args[key] = value.clone();
    }
    args
}

#[tokio::test]
async fn edit_accepts_every_property_filled_when_the_single_fields_change_nothing() {
    // Given: the seven-key calls recorded from models that fill every property: a
    // placeholder, a copy of edits[0], and a replacement of a text with itself.
    for single in [
        json!({"old_string": "unused", "new_string": "", "replace_all": false}),
        json!({"old_string": "one", "new_string": "ONE", "replace_all": false}),
        json!({"old_string": "same", "new_string": "same", "replace_all": false}),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        std::fs::write(&path, "one\n").unwrap();
        let args = all_seven_keys(&path, single.clone());
        assert_eq!(args.as_object().unwrap().len(), 7, "{args}");

        // When: the executor validates and runs the call.
        let result = executor()
            .execute(&context(), "edit", "c1", args)
            .await
            .unwrap();

        // Then: edits is applied once and the ignored fields are named.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ONE\n", "{single}");
        assert!(
            result
                .content
                .contains("ignored old_string/new_string/replace_all"),
            "{}",
            result.content
        );
    }
}

#[tokio::test]
async fn edit_reads_null_optional_fields_as_omitted() {
    // Given: calls that send null for every property they do not use.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("note.md");
    std::fs::write(&path, "one\ntwo\n").unwrap();
    let executor = executor();

    // When: one call uses edits and the other the single fields.
    let several = executor
        .execute(
            &context(),
            "edit",
            "c1",
            json!({
                "path": path,
                "edits": [{"old_string": "one", "new_string": "ONE", "replace_all": null}],
                "old_string": null, "new_string": null, "replace_all": null,
                "justification": null, "sandbox_access": null,
            }),
        )
        .await
        .unwrap();
    executor
        .execute(
            &context(),
            "edit",
            "c2",
            json!({
                "path": path, "edits": null,
                "old_string": "two", "new_string": "TWO", "replace_all": null,
                "justification": null, "sandbox_access": null,
            }),
        )
        .await
        .unwrap();

    // Then: both run as if the null fields were absent.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "ONE\nTWO\n");
    assert!(!several.content.contains("ignored"), "{}", several.content);
}

#[tokio::test]
async fn edit_still_rejects_null_required_fields() {
    // Given: a file and calls with null in a required field.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("note.md");
    std::fs::write(&path, "one\n").unwrap();
    for args in [
        json!({"path": null, "old_string": "one", "new_string": "ONE"}),
        json!({"path": path, "edits": [{"old_string": "one", "new_string": null}]}),
    ] {
        // When: the executor validates the call.
        let error = executor()
            .execute(&context(), "edit", "c1", args.clone())
            .await
            .unwrap_err();

        // Then: the required field is not dropped, and the file is unchanged.
        assert!(
            matches!(error, ToolError::InvalidArgs { .. }),
            "{args}: {error:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\n");
    }
}
