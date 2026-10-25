use super::*;
use crate::Hunk;
use crate::apply_patch;
use crate::parse_patch;
use std::fs;
use tempfile::tempdir;

// Keep upstream's unified-diff assertions without expanding the production API:
// the engine exposes final contents, not a diff-generation service.
#[derive(Debug, PartialEq, Eq)]
struct ApplyPatchFileUpdate {
    unified_diff: String,
    original_content: String,
    content: String,
}

fn unified_diff_from_chunks(
    path: &std::path::Path,
    chunks: &[UpdateFileChunk],
) -> Result<ApplyPatchFileUpdate, ApplyPatchError> {
    let original_content = fs::read_to_string(path)?;
    let content =
        derive_new_contents_from_chunks(&original_content, &path.to_string_lossy(), chunks)?;
    let unified_diff = similar::TextDiff::from_lines(&original_content, &content)
        .unified_diff()
        .context_radius(1)
        .to_string();
    Ok(ApplyPatchFileUpdate {
        unified_diff,
        original_content,
        content,
    })
}

fn wrap_patch(body: &str) -> String {
    format!("*** Begin Patch\n{body}\n*** End Patch")
}

#[test]
fn test_unified_diff() {
    // Start with a file containing four lines.
    let dir = tempdir().unwrap();
    let path = dir.path().join("multi.txt");
    fs::write(&path, "foo\nbar\nbaz\nqux\n").unwrap();
    let patch = wrap_patch(&format!(
        r#"*** Update File: {}
@@
 foo
-bar
+BAR
@@
 baz
-qux
+QUX"#,
        path.display()
    ));
    let patch = parse_patch(&patch).unwrap();

    let update_file_chunks = match patch.hunks.as_slice() {
        [Hunk::UpdateFile { chunks, .. }] => chunks,
        _ => panic!("Expected a single UpdateFile hunk"),
    };
    let diff = unified_diff_from_chunks(&path, update_file_chunks).unwrap();
    let expected_diff = r#"@@ -1,4 +1,4 @@
 foo
-bar
+BAR
 baz
-qux
+QUX
"#;
    let expected = ApplyPatchFileUpdate {
        unified_diff: expected_diff.to_string(),
        original_content: "foo\nbar\nbaz\nqux\n".to_string(),
        content: "foo\nBAR\nbaz\nQUX\n".to_string(),
    };
    assert_eq!(expected, diff);
}

#[test]
fn test_unified_diff_first_line_replacement() {
    // Replace the very first line of the file.
    let dir = tempdir().unwrap();
    let path = dir.path().join("first.txt");
    fs::write(&path, "foo\nbar\nbaz\n").unwrap();

    let patch = wrap_patch(&format!(
        r#"*** Update File: {}
@@
-foo
+FOO
 bar
"#,
        path.display()
    ));

    let patch = parse_patch(&patch).unwrap();
    let chunks = match patch.hunks.as_slice() {
        [Hunk::UpdateFile { chunks, .. }] => chunks,
        _ => panic!("Expected a single UpdateFile hunk"),
    };

    let diff = unified_diff_from_chunks(&path, chunks).unwrap();
    let expected_diff = r#"@@ -1,2 +1,2 @@
-foo
+FOO
 bar
"#;
    let expected = ApplyPatchFileUpdate {
        unified_diff: expected_diff.to_string(),
        original_content: "foo\nbar\nbaz\n".to_string(),
        content: "FOO\nbar\nbaz\n".to_string(),
    };
    assert_eq!(expected, diff);
}

#[test]
fn test_unified_diff_last_line_replacement() {
    // Replace the very last line of the file.
    let dir = tempdir().unwrap();
    let path = dir.path().join("last.txt");
    fs::write(&path, "foo\nbar\nbaz\n").unwrap();

    let patch = wrap_patch(&format!(
        r#"*** Update File: {}
@@
 foo
 bar
-baz
+BAZ
"#,
        path.display()
    ));

    let patch = parse_patch(&patch).unwrap();
    let chunks = match patch.hunks.as_slice() {
        [Hunk::UpdateFile { chunks, .. }] => chunks,
        _ => panic!("Expected a single UpdateFile hunk"),
    };

    let diff = unified_diff_from_chunks(&path, chunks).unwrap();
    let expected_diff = r#"@@ -2,2 +2,2 @@
 bar
-baz
+BAZ
"#;
    let expected = ApplyPatchFileUpdate {
        unified_diff: expected_diff.to_string(),
        original_content: "foo\nbar\nbaz\n".to_string(),
        content: "foo\nbar\nBAZ\n".to_string(),
    };
    assert_eq!(expected, diff);
}

#[test]
fn test_unified_diff_insert_at_eof() {
    // Insert a new line at end-of-file.
    let dir = tempdir().unwrap();
    let path = dir.path().join("insert.txt");
    fs::write(&path, "foo\nbar\nbaz\n").unwrap();

    let patch = wrap_patch(&format!(
        r#"*** Update File: {}
@@
+quux
*** End of File
"#,
        path.display()
    ));

    let patch = parse_patch(&patch).unwrap();
    let chunks = match patch.hunks.as_slice() {
        [Hunk::UpdateFile { chunks, .. }] => chunks,
        _ => panic!("Expected a single UpdateFile hunk"),
    };

    let diff = unified_diff_from_chunks(&path, chunks).unwrap();
    let expected_diff = r#"@@ -3 +3,2 @@
 baz
+quux
"#;
    let expected = ApplyPatchFileUpdate {
        unified_diff: expected_diff.to_string(),
        original_content: "foo\nbar\nbaz\n".to_string(),
        content: "foo\nbar\nbaz\nquux\n".to_string(),
    };
    assert_eq!(expected, diff);
}

#[test]
fn test_unified_diff_interleaved_changes() {
    // Original file with six lines.
    let dir = tempdir().unwrap();
    let path = dir.path().join("interleaved.txt");
    fs::write(&path, "a\nb\nc\nd\ne\nf\n").unwrap();

    // Patch replaces two separate lines and appends a new one at EOF using
    // three distinct chunks.
    let patch_body = format!(
        r#"*** Update File: {}
@@
 a
-b
+B
@@
 d
-e
+E
@@
 f
+g
*** End of File"#,
        path.display()
    );
    let patch = wrap_patch(&patch_body);

    // Extract chunks then build the unified diff.
    let parsed = parse_patch(&patch).unwrap();
    let chunks = match parsed.hunks.as_slice() {
        [Hunk::UpdateFile { chunks, .. }] => chunks,
        _ => panic!("Expected a single UpdateFile hunk"),
    };

    let diff = unified_diff_from_chunks(&path, chunks).unwrap();

    let expected_diff = r#"@@ -1,6 +1,7 @@
 a
-b
+B
 c
 d
-e
+E
 f
+g
"#;

    let expected = ApplyPatchFileUpdate {
        unified_diff: expected_diff.to_string(),
        original_content: "a\nb\nc\nd\ne\nf\n".to_string(),
        content: "a\nB\nc\nd\nE\nf\ng\n".to_string(),
    };

    assert_eq!(expected, diff);

    apply_patch(&patch, Some(dir.path())).unwrap();
    let contents = fs::read_to_string(path).unwrap();
    assert_eq!(
        contents,
        r#"a
B
c
d
E
f
g
"#
    );
}
