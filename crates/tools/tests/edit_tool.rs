//! 部分置換と、新規作成・全文書き込みの誤用を検証する。

use serde_json::json;
use tools::{Edit, Tool, ToolError};

#[tokio::test]
async fn edit_replaces_first_occurrence_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("twice.txt");
    std::fs::write(&path, "alpha TARGET beta TARGET gamma\n").unwrap();
    Edit.execute(json!({"path": path, "old_string": "TARGET", "new_string": "REPLACED"}))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "alpha REPLACED beta TARGET gamma\n"
    );
}

#[tokio::test]
async fn edit_missing_target_leaves_file_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.txt");
    std::fs::write(&path, "original").unwrap();
    let error = Edit
        .execute(json!({"path": path, "old_string": "absent", "new_string": "x"}))
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::EditTargetNotFound { .. }));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "original");
}

#[tokio::test]
async fn edit_does_not_create_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent.txt");
    let error = Edit
        .execute(json!({"path": path, "old_string": "x", "new_string": "y"}))
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::PathNotFound { .. }));
    assert!(!path.exists());
}

#[tokio::test]
async fn edit_requires_non_empty_old_string_without_touching_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.txt");
    std::fs::write(&path, "original").unwrap();
    for args in [
        json!({"path": path, "new_string": "replaced"}),
        json!({"path": path, "old_string": "", "new_string": "prepended"}),
        json!({"path": path, "old_string": null, "new_string": "replaced"}),
    ] {
        assert!(matches!(
            Edit.execute(args).await.unwrap_err(),
            ToolError::InvalidArgs { .. }
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
    }
}

#[tokio::test]
async fn edit_accepts_empty_replacement_for_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.txt");
    std::fs::write(&path, "remove日本語keep").unwrap();
    Edit.execute(json!({"path": path, "old_string": "remove日本語", "new_string": ""}))
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), "keep");
}
