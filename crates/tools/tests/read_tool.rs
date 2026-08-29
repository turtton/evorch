//! [`Read`] ツールの統合テスト。

use std::fs;

use tempfile::tempdir;
use tools::{Read, Tool, ToolError};

// Given: 日本語を含む複数行の UTF-8 ファイル / When: read を実行 / Then: 内容が装飾なしで逐語的に返る
#[tokio::test]
async fn read_returns_file_content() {
    let dir = tempdir().expect("一時ディレクトリの作成に失敗");
    let path = dir.path().join("sample.txt");
    let content = "1 行目\nsecond line\n三行目: 日本語\n";
    fs::write(&path, content).expect("テストファイルの書き込みに失敗");

    let result = Read
        .execute(serde_json::json!({ "path": path.display().to_string() }))
        .await
        .expect("既存ファイルの読み取りは成功するべき");

    assert!(!result.is_error);
    assert_eq!(result.content, content);
}

// Given: 存在しないパス / When: read を実行 / Then: PathNotFound が返る
#[tokio::test]
async fn read_missing_path_is_path_not_found() {
    let dir = tempdir().expect("一時ディレクトリの作成に失敗");
    let missing = dir.path().join("missing.txt");
    let expected = missing.display().to_string();

    let error = Read
        .execute(serde_json::json!({ "path": expected.clone() }))
        .await
        .expect_err("存在しないパスはエラーになるべき");

    assert!(
        matches!(&error, ToolError::PathNotFound { path } if *path == expected),
        "実際のエラー: {error:?}"
    );
}

// Given: ディレクトリのパス / When: read を実行 / Then: NotAFile が返る
#[tokio::test]
async fn read_directory_is_not_a_file() {
    let dir = tempdir().expect("一時ディレクトリの作成に失敗");
    let expected = dir.path().display().to_string();

    let error = Read
        .execute(serde_json::json!({ "path": expected.clone() }))
        .await
        .expect_err("ディレクトリの読み取りはエラーになるべき");

    assert!(
        matches!(&error, ToolError::NotAFile { path } if *path == expected),
        "実際のエラー: {error:?}"
    );
}

// Given: 非 UTF-8 バイト列のファイル / When: read を実行 / Then: Io エラーが返る
#[tokio::test]
async fn read_non_utf8_content_is_io_error() {
    let dir = tempdir().expect("一時ディレクトリの作成に失敗");
    let path = dir.path().join("binary.bin");
    fs::write(&path, [0xFF, 0xFE, b'a']).expect("テストファイルの書き込みに失敗");

    let error = Read
        .execute(serde_json::json!({ "path": path.display().to_string() }))
        .await
        .expect_err("非 UTF-8 の内容はエラーになるべき");

    assert!(
        matches!(&error, ToolError::Io { .. }),
        "実際のエラー: {error:?}"
    );
}
