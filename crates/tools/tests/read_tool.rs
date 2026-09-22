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

#[tokio::test]
async fn read_range_reports_next_line_and_stops_at_eof() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("range.txt");
    fs::write(&path, "first\nsecond\n三行目\nfourth").unwrap();
    let result = Read
        .execute(serde_json::json!({"path": path, "offset": 2, "limit": 2}))
        .await
        .unwrap();
    let detail = result.detail.unwrap();
    assert!(result.content.starts_with("second\n三行目\n"));
    assert_eq!(detail["next_offset"], 4);
    assert_eq!(detail["next_byte_offset"], 0);
    let last = Read
        .execute(serde_json::json!({"path": path, "offset": 4}))
        .await
        .unwrap();
    assert_eq!(last.content, "fourth");
    assert_eq!(last.detail.unwrap()["truncated"], false);
    assert_eq!(
        Read.execute(serde_json::json!({"path": path, "offset": 99}))
            .await
            .unwrap()
            .content,
        ""
    );
}

#[tokio::test]
async fn read_caps_line_limit_even_when_requested_larger() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("many.txt");
    fs::write(&path, "line\n".repeat(1000)).unwrap();
    let result = Read
        .execute(serde_json::json!({"path": path, "limit": 10000}))
        .await
        .unwrap();
    let detail = result.detail.unwrap();
    assert_eq!(detail["bytes_returned"], 1500);
    assert_eq!(detail["next_offset"], 301);
    assert!(result.content.starts_with(&"line\n".repeat(300)));
}

#[tokio::test]
async fn read_continues_long_unicode_line_without_loss_or_duplicates() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("long.txt");
    let content = format!("skip\n{}\nlast", "日本語🦀".repeat(6000));
    fs::write(&path, &content).unwrap();
    let mut offset = 2;
    let mut byte_offset = 0;
    let mut collected = String::new();
    loop {
        let result = Read
            .execute(
                serde_json::json!({"path": path, "offset": offset, "byte_offset": byte_offset}),
            )
            .await
            .unwrap();
        let detail = result.detail.unwrap();
        let bytes = detail["bytes_returned"].as_u64().unwrap() as usize;
        assert!(bytes <= tools::tools::read::MAX_READ_BYTES);
        collected.push_str(&result.content[..bytes]);
        if !detail["truncated"].as_bool().unwrap() {
            break;
        }
        offset = detail["next_offset"].as_u64().unwrap();
        byte_offset = detail["next_byte_offset"].as_u64().unwrap();
    }
    assert_eq!(collected, content.strip_prefix("skip\n").unwrap());
}

#[tokio::test]
async fn read_skips_giant_line_and_rejects_invalid_ranges() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("giant.txt");
    fs::write(&path, format!("{}\nnext\n", "x".repeat(2 * 1024 * 1024))).unwrap();
    let result = Read
        .execute(serde_json::json!({"path": path, "offset": 2}))
        .await
        .unwrap();
    assert_eq!(result.content, "next\n");
    for args in [
        serde_json::json!({"path": path, "offset": 0}),
        serde_json::json!({"path": path, "limit": -1}),
    ] {
        assert!(matches!(
            Read.execute(args).await.unwrap_err(),
            ToolError::InvalidArgs { .. }
        ));
    }
    assert!(
        Read.execute(serde_json::json!({"path": path, "offset": 2, "byte_offset": 6}))
            .await
            .is_err()
    );
}
