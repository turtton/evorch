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

#[tokio::test]
async fn write_returns_create_and_overwrite_diffs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write.txt");
    let created = Write
        .execute(json!({"path": path, "content": "first\n"}))
        .await
        .unwrap();
    assert!(
        created
            .content
            .starts_with(&format!("--- /dev/null\n+++ b/{}\n", path.display()))
    );
    assert!(created.content.contains("+first\n"));

    let overwritten = Write
        .execute(json!({"path": path, "content": "second\n"}))
        .await
        .unwrap();
    assert!(overwritten.content.starts_with(&format!(
        "--- a/{}\n+++ b/{}\n",
        path.display(),
        path.display()
    )));
    assert!(overwritten.content.contains("-first\n+second\n"));
}

#[tokio::test]
async fn write_preserves_replacement_of_non_utf8_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("binary.txt");
    std::fs::write(&path, [0xff]).unwrap();
    let result = Write
        .execute(json!({"path": path, "content": "valid"}))
        .await
        .unwrap();
    assert!(result.content.contains("diff unavailable"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "valid");
}

#[tokio::test]
async fn write_omits_diff_for_large_previous_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("large.txt");
    std::fs::write(&path, "x".repeat(2 * 1024 * 1024 + 1)).unwrap();
    let result = Write
        .execute(json!({"path": path, "content": "small"}))
        .await
        .unwrap();
    assert!(
        result
            .content
            .contains("diff unavailable: previous file exceeds")
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "small");
}
