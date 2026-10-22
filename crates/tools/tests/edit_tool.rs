//! 部分置換と、新規作成・全文書き込みの誤用を検証する。

use std::os::unix::fs::MetadataExt;

use serde_json::json;
use tools::{Edit, Tool, ToolError};

#[tokio::test]
async fn edit_rejects_an_ambiguous_target_with_its_lines_and_leaves_the_file_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("twice.txt");
    let original = "alpha TARGET\nbeta\nTARGET gamma\n";
    std::fs::write(&path, original).unwrap();
    let error = Edit
        .execute(json!({"path": path, "old_string": "TARGET", "new_string": "REPLACED"}))
        .await
        .unwrap_err();
    assert!(
        matches!(
            &error,
            ToolError::EditTargetNotUnique { edit: None, count: 2, lines, .. } if *lines == [1, 3]
        ),
        "{error:?}"
    );
    let message = error.to_string();
    assert!(message.contains("1, 3"), "{message}");
    assert!(message.contains("replace_all"), "{message}");
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
}

#[tokio::test]
async fn edit_treats_a_self_overlapping_match_as_ambiguous() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("overlap.txt");
    std::fs::write(&path, "}\n}\n}\n").unwrap();
    let error = Edit
        .execute(json!({"path": path, "old_string": "}\n}", "new_string": "]"}))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ToolError::EditTargetNotUnique { count: 2, lines, .. } if *lines == [1, 2]),
        "{error:?}"
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "}\n}\n}\n");
}

#[tokio::test]
async fn edit_replace_all_replaces_every_match() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("twice.txt");
    std::fs::write(&path, "alpha TARGET beta TARGET gamma\n").unwrap();
    let result = Edit
        .execute(json!({"path": path, "old_string": "TARGET", "new_string": "REPLACED", "replace_all": true}))
        .await
        .unwrap();
    assert!(
        result
            .content
            .contains("+alpha REPLACED beta REPLACED gamma\n")
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "alpha REPLACED beta REPLACED gamma\n"
    );
    let error = Edit
        .execute(
            json!({"path": path, "old_string": "TARGET", "new_string": "x", "replace_all": true}),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, ToolError::EditTargetNotFound { edit: None, .. }),
        "{error:?}"
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
    assert!(matches!(
        error,
        ToolError::EditTargetNotFound { edit: None, .. }
    ));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "original");
}

#[tokio::test]
async fn edit_does_not_create_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent.txt");
    for args in [
        json!({"path": path, "old_string": "x", "new_string": "y"}),
        json!({"path": path, "edits": [{"old_string": "x", "new_string": "y"}]}),
    ] {
        let error = Edit.execute(args).await.unwrap_err();
        assert!(matches!(error, ToolError::PathNotFound { .. }), "{error:?}");
        assert!(!path.exists());
    }
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
        json!({"path": path, "edits": [{"old_string": "", "new_string": "prepended"}]}),
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

#[tokio::test]
async fn edit_returns_the_actual_file_diff() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("edit.txt");
    std::fs::write(&path, "keep\nold\nkeep\n").unwrap();
    let result = Edit
        .execute(json!({"path": path, "old_string": "old", "new_string": "new"}))
        .await
        .unwrap();

    assert!(result.content.starts_with(&format!(
        "--- a/{}\n+++ b/{}\n",
        path.display(),
        path.display()
    )));
    assert!(result.content.contains("@@ -1,3 +1,3 @@"));
    assert!(result.content.contains("-old\n+new\n"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "keep\nnew\nkeep\n");
}

#[tokio::test]
async fn edit_noop_reports_that_no_bytes_changed_without_rewriting_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("same.txt");
    std::fs::write(&path, "same\nother\n").unwrap();
    let inode = std::fs::metadata(&path).unwrap().ino();
    for args in [
        json!({"path": path, "old_string": "same", "new_string": "same"}),
        json!({"path": path, "edits": [
            {"old_string": "same", "new_string": "same"},
            {"old_string": "other", "new_string": "other"}
        ]}),
    ] {
        let result = Edit.execute(args).await.unwrap();
        assert_eq!(result.content, format!("No changes to {}", path.display()));
        // A rewrite would replace the file through a rename.
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
    }
    // A no-op still has to identify one location.
    std::fs::write(&path, "same same\n").unwrap();
    let error = Edit
        .execute(json!({"path": path, "old_string": "same", "new_string": "same"}))
        .await
        .unwrap_err();
    assert!(
        matches!(error, ToolError::EditTargetNotUnique { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn edits_match_the_original_contents_and_report_one_diff() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("multi.txt");
    std::fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();
    let result = Edit
        .execute(json!({"path": path, "edits": [
            {"old_string": "three", "new_string": "3"},
            // Matched against the original file, so the "one" this edit
            // inserts is never seen by the edit below.
            {"old_string": "two", "new_string": "one"},
            {"old_string": "one\n", "new_string": "1\n"},
        ]}))
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "1\none\n3\nfour\n");
    assert_eq!(result.content.matches("--- a/").count(), 1);
    for line in ["+1\n", "-two\n", "-three\n", "+3\n"] {
        assert!(result.content.contains(line), "{}", result.content);
    }
}

#[tokio::test]
async fn edits_accept_replace_all_per_item() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("multi.txt");
    std::fs::write(&path, "a a a\nunique\n").unwrap();
    Edit.execute(json!({"path": path, "edits": [
        {"old_string": "a", "new_string": "b", "replace_all": true},
        {"old_string": "unique", "new_string": "UNIQUE", "replace_all": false},
    ]}))
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), "b b b\nUNIQUE\n");
}

#[tokio::test]
async fn edits_are_all_or_nothing_and_name_the_failing_edit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("multi.txt");
    let original = "one\ntwo\ntwo\n";
    std::fs::write(&path, original).unwrap();
    let error = Edit
        .execute(json!({"path": path, "edits": [
            {"old_string": "one", "new_string": "1"},
            {"old_string": "absent", "new_string": "x"},
        ]}))
        .await
        .unwrap_err();
    assert!(
        matches!(error, ToolError::EditTargetNotFound { edit: Some(1), .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("edits[1]"), "{error}");
    let error = Edit
        .execute(json!({"path": path, "edits": [
            {"old_string": "one", "new_string": "1"},
            {"old_string": "two", "new_string": "2"},
        ]}))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ToolError::EditTargetNotUnique { edit: Some(1), count: 2, lines, .. } if *lines == [2, 3]),
        "{error:?}"
    );
    assert!(error.to_string().contains("edits[1]"), "{error}");
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
}

#[tokio::test]
async fn edits_reject_overlapping_matches() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("multi.txt");
    let original = "abcdef\nsame\n";
    std::fs::write(&path, original).unwrap();
    for (edits, first, second) in [
        (
            json!([{"old_string": "abc", "new_string": "x"}, {"old_string": "cde", "new_string": "y"}]),
            "edits[0]",
            "edits[1]",
        ),
        (
            json!([{"old_string": "same", "new_string": "x"}, {"old_string": "f", "new_string": "f"}, {"old_string": "same", "new_string": "y"}]),
            "edits[0]",
            "edits[2]",
        ),
        (
            json!([{"old_string": "bcd", "new_string": "x"}, {"old_string": "c", "new_string": "y", "replace_all": true}]),
            "edits[0]",
            "edits[1]",
        ),
    ] {
        let error = Edit
            .execute(json!({"path": path, "edits": edits}))
            .await
            .unwrap_err();
        let ToolError::InvalidArgs { detail } = &error else {
            panic!("expected InvalidArgs, got {error:?}");
        };
        assert!(
            detail.contains(first) && detail.contains(second) && detail.contains("overlap"),
            "{detail}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}

#[tokio::test]
async fn edit_rejects_ambiguous_argument_shapes_without_touching_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.txt");
    std::fs::write(&path, "original").unwrap();
    let edits = json!([{"old_string": "original", "new_string": "edited"}]);
    for args in [
        json!({"path": path, "old_string": "original", "new_string": "x", "edits": edits}),
        // Top-level replace_all only applies to old_string/new_string.
        json!({"path": path, "replace_all": true, "edits": edits}),
        json!({"path": path}),
        json!({"path": path, "edits": []}),
        json!({"path": path, "edits": [{"old_string": "original"}]}),
        json!({"path": path, "edits": [{"old_string": "original", "new_string": "x", "replace_all": "yes"}]}),
        json!({"path": path, "old_string": "original", "new_string": "x", "replace_all": "yes"}),
    ] {
        let error = Edit.execute(args.clone()).await.unwrap_err();
        assert!(
            matches!(error, ToolError::InvalidArgs { .. }),
            "{args}: {error:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
    }
}

#[tokio::test]
async fn edit_matches_lf_text_in_a_crlf_file_and_writes_crlf() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crlf.txt");
    std::fs::write(&path, "a\r\nb\r\nc\r\n").unwrap();
    Edit.execute(json!({"path": path, "old_string": "a\nb", "new_string": "x\ny\nz"}))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "x\r\ny\r\nz\r\nc\r\n"
    );
}

#[tokio::test]
async fn edit_writes_inserted_lines_as_crlf_only_in_a_crlf_only_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crlf.txt");
    std::fs::write(&path, "a\r\nb\r\nc\r\n").unwrap();
    Edit.execute(json!({"path": path, "edits": [
        {"old_string": "a", "new_string": "a\nafter a"},
        {"old_string": "b\nc", "new_string": "B\nC"},
    ]}))
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "a\r\nafter a\r\nB\r\nC\r\n"
    );
    // A file that already mixes line endings has no single style to follow.
    std::fs::write(&path, "a\r\nb\nc\r\n").unwrap();
    Edit.execute(json!({"path": path, "old_string": "a", "new_string": "a\nafter a"}))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "a\nafter a\r\nb\nc\r\n"
    );
}

#[tokio::test]
async fn edit_preserves_a_byte_order_mark_and_untouched_line_endings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bom.txt");
    std::fs::write(&path, "\u{feff}first\r\nmiddle\nlast\r\n").unwrap();
    Edit.execute(json!({"path": path, "edits": [
        {"old_string": "first", "new_string": "FIRST"},
        {"old_string": "last", "new_string": "LAST"},
    ]}))
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "\u{feff}FIRST\r\nmiddle\nLAST\r\n"
    );
}

#[test]
fn schema_accepts_single_and_multiple_replacements_only_in_their_documented_shape() {
    let validator = jsonschema::validator_for(&Edit.schema()).unwrap();
    for valid in [
        json!({"path": "a", "old_string": "x", "new_string": "y"}),
        json!({"path": "a", "old_string": "x", "new_string": "y", "replace_all": true}),
        json!({"path": "a", "edits": [{"old_string": "x", "new_string": "y"}, {"old_string": "z", "new_string": "", "replace_all": true}]}),
    ] {
        assert!(validator.is_valid(&valid), "{valid}");
    }
    for invalid in [
        json!({"path": "a", "edits": []}),
        json!({"path": "a", "edits": [{"old_string": "", "new_string": "y"}]}),
        json!({"path": "a", "edits": [{"old_string": "x"}]}),
        json!({"path": "a", "edits": [{"old_string": "x", "new_string": "y", "path": "b"}]}),
        json!({"path": "a", "old_string": "x", "new_string": "y", "replace_all": "yes"}),
    ] {
        assert!(!validator.is_valid(&invalid), "{invalid}");
    }
}
