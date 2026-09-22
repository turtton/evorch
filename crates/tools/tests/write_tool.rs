use serde_json::json;
use tools::{Tool, ToolError, Write};

#[tokio::test]
async fn write_creates_then_replaces_entire_file_byte_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("created.txt");
    for content in [
        "日本語\n<system-reminder>raw marker</system-reminder>\n",
        "replacement",
        "",
    ] {
        let result = Write
            .execute(json!({"path": path, "content": content}))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(std::fs::read(&path).unwrap(), content.as_bytes());
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "atomic temp file must not remain"
        );
    }
}

#[tokio::test]
async fn write_missing_parent_dir_is_io_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent").join("file.txt");
    let error = Write
        .execute(json!({"path": path, "content": "content"}))
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::Io { .. }));
    assert!(!path.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn atomic_write_and_edit_preserve_existing_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("script");
    std::fs::write(&path, "old").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o750)).unwrap();
    Write
        .execute(json!({"path": path, "content": "new"}))
        .await
        .unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o750
    );
    tools::Edit
        .execute(json!({"path": path, "old_string": "new", "new_string": "edited"}))
        .await
        .unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o750
    );
}
