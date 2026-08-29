//! edit ツールの統合テスト。
//!
//! `old_string` 省略時のファイル全体書き込みと、指定時の最初の一致箇所の置換、
//! ならびにエラー経路と ADR 0008 のバイト一致書き込みを検証する。

use tools::{Edit, Tool, ToolError};

/// 必須プロパティと任意の `old_string` から `execute` へ渡す引数を組み立てる。
fn args(path: &str, old_string: Option<&str>, new_string: &str) -> serde_json::Value {
    let mut value = serde_json::json!({
        "path": path,
        "new_string": new_string,
    });
    if let Some(old_string) = old_string {
        value["old_string"] = serde_json::Value::String(old_string.to_string());
    }
    value
}

// Given: 空の一時ディレクトリ / When: old_string 省略で edit を実行 / Then: ファイルがバイト一致で新規作成される
#[tokio::test]
async fn edit_whole_file_write_creates_file_atomically() {
    // Given: 空の一時ディレクトリと未存在のファイルパス
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("created.txt");
    let path_text = path.to_str().unwrap();

    // When: old_string を省略して edit を実行
    let result = Edit
        .execute(args(path_text, None, "hello\nworld\n"))
        .await
        .unwrap();

    // Then: 正常終了であり、ファイル内容がバイト一致する
    assert!(!result.is_error);
    assert_eq!(std::fs::read(&path).unwrap(), b"hello\nworld\n");
}

// Given: 既存ファイル / When: old_string 省略で別内容の edit を実行 / Then: ファイル全体が新しい内容へ置き換わる
#[tokio::test]
async fn edit_whole_file_overwrites_existing() {
    // Given: 既存内容を持つファイル
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("existing.txt");
    std::fs::write(&path, b"old content\n").unwrap();
    let path_text = path.to_str().unwrap();

    // When: old_string を省略して別内容で edit を実行
    let result = Edit
        .execute(args(path_text, None, "new content"))
        .await
        .unwrap();

    // Then: 正常終了であり、ファイル全体が完全に置き換わる
    assert!(!result.is_error);
    assert_eq!(std::fs::read(&path).unwrap(), b"new content");
}

// Given: 同一文字列が 2 回出現するファイル / When: old_string 指定で edit を実行 / Then: 最初の 1 箇所のみ置換される
#[tokio::test]
async fn edit_replaces_first_occurrence_only() {
    // Given: 置換対象が 2 回出現するファイル
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("twice.txt");
    std::fs::write(&path, b"alpha TARGET beta TARGET gamma\n").unwrap();
    let path_text = path.to_str().unwrap();

    // When: old_string を指定して edit を実行
    let result = Edit
        .execute(args(path_text, Some("TARGET"), "REPLACED"))
        .await
        .unwrap();

    // Then: 最初の出現のみが置換され、2 回目はそのまま残る
    assert!(!result.is_error);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"alpha REPLACED beta TARGET gamma\n"
    );
}

// Given: old_string が本文に存在しないファイル / When: old_string 指定で edit を実行 / Then: EditTargetNotFound で失敗し内容は不変
#[tokio::test]
async fn edit_old_string_missing_is_edit_target_not_found() {
    // Given: 置換対象を含まない既存ファイル
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("no_target.txt");
    std::fs::write(&path, b"nothing matches here\n").unwrap();
    let path_text = path.to_str().unwrap();

    // When: 存在しない old_string を指定して edit を実行
    let error = Edit
        .execute(args(path_text, Some("absent"), "x"))
        .await
        .unwrap_err();

    // Then: EditTargetNotFound がパス付きで返り、ファイル内容は不変である
    assert_eq!(
        error,
        ToolError::EditTargetNotFound {
            path: path_text.to_string()
        }
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"nothing matches here\n");
}

// Given: 存在しないファイル / When: old_string 指定で edit を実行 / Then: PathNotFound で失敗しファイルは作成されない
#[tokio::test]
async fn edit_missing_file_with_old_string_is_path_not_found() {
    // Given: 一時ディレクトリ内の未存在パス
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent.txt");
    let path_text = path.to_str().unwrap();

    // When: old_string を指定して edit を実行
    let error = Edit
        .execute(args(path_text, Some("x"), "y"))
        .await
        .unwrap_err();

    // Then: PathNotFound がパス付きで返り、ファイルは作成されない
    assert_eq!(
        error,
        ToolError::PathNotFound {
            path: path_text.to_string()
        }
    );
    assert!(!path.exists());
}

// Given: 親ディレクトリが存在しないパス / When: old_string 省略で edit を実行 / Then: Io エラーで失敗する
#[tokio::test]
async fn edit_missing_parent_dir_is_io_error() {
    // Given: 未存在の親ディレクトリ配下のパス
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nonexistent_dir").join("file.txt");

    // When: old_string を省略して edit を実行
    let error = Edit
        .execute(args(path.to_str().unwrap(), None, "content"))
        .await
        .unwrap_err();

    // Then: Io エラーが返り、ディレクトリもファイルも作成されない
    assert!(matches!(error, ToolError::Io { .. }));
    assert!(!path.exists());
}

// Given: リテラルの制御マーカーを含む new_string / When: old_string 省略で edit を実行 / Then: マーカーがエスケープされずバイト一致で書き込まれる
#[tokio::test]
async fn edit_writes_marker_content_byte_exact() {
    // Given: リテラルの <system-reminder> を含む書き込み内容
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("marker.txt");
    let path_text = path.to_str().unwrap();
    let content = "before\n<system-reminder>raw marker</system-reminder>\nafter\n";

    // When: old_string を省略して edit を実行
    let result = Edit.execute(args(path_text, None, content)).await.unwrap();

    // Then: ディスク上のバイトが生のマーカーを含む内容と完全一致する
    assert!(!result.is_error);
    let written = std::fs::read(&path).unwrap();
    assert_eq!(written, content.as_bytes());
    assert!(
        String::from_utf8(written)
            .unwrap()
            .contains("<system-reminder>")
    );
}
