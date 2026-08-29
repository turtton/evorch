//! [`Grep`] ツールの統合テスト。

use std::fs;

use tempfile::tempdir;
use tools::{Grep, Tool, ToolError};

// Given: 複数行ファイルと一致するパターン / When: grep を実行 / Then: 一致行が `path:行番号:行` 形式で行番号順に返る
#[tokio::test]
async fn grep_matches_lines_in_file_with_line_numbers() {
    let dir = tempdir().expect("一時ディレクトリの作成に失敗");
    let path = dir.path().join("notes.txt");
    fs::write(&path, "りんご\napple pie\nno match\napple again\n")
        .expect("テストファイルの書き込みに失敗");
    let path = path.display().to_string();

    let result = Grep
        .execute(serde_json::json!({ "pattern": "apple", "path": path.clone() }))
        .await
        .expect("ファイル内検索は成功するべき");

    assert!(!result.is_error);
    let expected = format!("{path}:2:apple pie\n{path}:4:apple again");
    assert_eq!(result.content, expected);
}

// Given: 入れ子のディレクトリ、.git 内の一致、複数ファイルの一致 / When: grep を実行 / Then: ソート済みの決定論的順序で返り .git の一致は含まれない
#[tokio::test]
async fn grep_recurses_directories_sorted_and_skips_git_dir() {
    let dir = tempdir().expect("一時ディレクトリの作成に失敗");
    let root = dir.path();

    // 作成順とソート順がずれるように配置する（read_dir の返す順序に依存しないことの担保）。
    fs::write(root.join("b_second.txt"), "needle in b\n").expect("書き込みに失敗");
    let git_dir = root.join(".git");
    fs::create_dir(&git_dir).expect("作成に失敗");
    fs::write(git_dir.join("config"), "needle decoy\n").expect("書き込みに失敗");
    let a_dir = root.join("a_dir");
    fs::create_dir(&a_dir).expect("作成に失敗");
    fs::write(a_dir.join("second.txt"), "needle in a2\n").expect("書き込みに失敗");
    fs::write(a_dir.join("first.txt"), "needle in a1\n").expect("書き込みに失敗");
    let root = root.display().to_string();

    let result = Grep
        .execute(serde_json::json!({ "pattern": "needle", "path": root.clone() }))
        .await
        .expect("ディレクトリ検索は成功するべき");

    assert!(!result.is_error);
    let expected = format!(
        "{root}/a_dir/first.txt:1:needle in a1\n\
         {root}/a_dir/second.txt:1:needle in a2\n\
         {root}/b_second.txt:1:needle in b"
    );
    assert_eq!(result.content, expected);
    assert!(
        !result.content.contains("decoy"),
        ".git 内の一致は含まれないべき"
    );
}

// Given: 存在しないパス / When: grep を実行 / Then: PathNotFound が返る
#[tokio::test]
async fn grep_missing_path_is_path_not_found() {
    let dir = tempdir().expect("一時ディレクトリの作成に失敗");
    let missing = dir.path().join("missing.txt");
    let expected = missing.display().to_string();

    let error = Grep
        .execute(serde_json::json!({ "pattern": "needle", "path": expected.clone() }))
        .await
        .expect_err("存在しないパスはエラーになるべき");

    assert!(
        matches!(&error, ToolError::PathNotFound { path } if *path == expected),
        "実際のエラー: {error:?}"
    );
}

// Given: 不正な正規表現と存在するファイル / When: grep を実行 / Then: InvalidPattern が返る
#[tokio::test]
async fn grep_invalid_regex_is_invalid_pattern() {
    let dir = tempdir().expect("一時ディレクトリの作成に失敗");
    let path = dir.path().join("notes.txt");
    fs::write(&path, "content\n").expect("テストファイルの書き込みに失敗");

    let error = Grep
        .execute(serde_json::json!({ "pattern": "[", "path": path.display().to_string() }))
        .await
        .expect_err("不正な正規表現はエラーになるべき");

    assert!(
        matches!(&error, ToolError::InvalidPattern { detail } if !detail.is_empty()),
        "実際のエラー: {error:?}"
    );
}

// Given: 一致行のないファイル / When: grep を実行 / Then: 空本文の正常結果が返る
#[tokio::test]
async fn grep_no_match_returns_empty_success() {
    let dir = tempdir().expect("一時ディレクトリの作成に失敗");
    let path = dir.path().join("notes.txt");
    fs::write(&path, "alpha\nbeta\n").expect("テストファイルの書き込みに失敗");

    let result = Grep
        .execute(serde_json::json!({ "pattern": "gamma", "path": path.display().to_string() }))
        .await
        .expect("一致なしはエラーではないべき");

    assert_eq!(result.content, "");
    assert!(!result.is_error);
}

// Given: 非 UTF-8 ファイルと一致する UTF-8 ファイルを含むディレクトリ / When: grep を実行 / Then: UTF-8 側の一致のみが返りエラーにはならない
#[tokio::test]
async fn grep_skips_non_utf8_files_in_dir() {
    let dir = tempdir().expect("一時ディレクトリの作成に失敗");
    let root = dir.path();
    fs::write(root.join("binary.bin"), [0xFF, 0xFE, 0x00, b'a', 0x80])
        .expect("テストファイルの書き込みに失敗");
    fs::write(root.join("text.txt"), "needle here\n").expect("テストファイルの書き込みに失敗");
    let root = root.display().to_string();

    let result = Grep
        .execute(serde_json::json!({ "pattern": "needle", "path": root.clone() }))
        .await
        .expect("ディレクトリ検索は成功するべき");

    assert!(!result.is_error);
    assert_eq!(result.content, format!("{root}/text.txt:1:needle here"));
}

// Given: 非 UTF-8 ファイルが単一の明示的引数として渡される / When: grep を実行 / Then: Io エラーが返る
#[tokio::test]
async fn grep_explicit_non_utf8_file_is_io_error() {
    let dir = tempdir().expect("一時ディレクトリの作成に失敗");
    let path = dir.path().join("binary.bin");
    fs::write(&path, [0xFF, 0xFE]).expect("テストファイルの書き込みに失敗");

    let error = Grep
        .execute(serde_json::json!({ "pattern": "needle", "path": path.display().to_string() }))
        .await
        .expect_err("明示的な非 UTF-8 ファイルはエラーになるべき");

    assert!(
        matches!(&error, ToolError::Io { .. }),
        "実際のエラー: {error:?}"
    );
}
