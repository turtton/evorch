//! Synchronous ports of the application assertions in upstream `lib.rs`.
//! File-update diff assertions live in `src/file_update_tests.rs`; executor-only
//! inexact deltas are replaced by the read-only planning guarantees in `engine`.

use std::fs;

use apply_patch::{ChangeKind, apply_patch};

fn patch(body: &str) -> String {
    format!("*** Begin Patch\n{body}\n*** End Patch")
}

#[test]
fn test_add_file_hunk_creates_file_with_contents() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("add.txt");
    let applied = apply_patch(
        &patch(&format!("*** Add File: {}\n+ab\n+cd", path.display())),
        Some(dir.path()),
    )
    .unwrap();
    assert_eq!(
        applied.summary,
        format!(
            "Success. Updated the following files:\nA {}\n",
            path.display()
        )
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "ab\ncd\n");
    assert_eq!(
        applied.changes[0].change,
        ChangeKind::Added {
            content: "ab\ncd\n".into(),
            overwritten: None
        }
    );
}

#[test]
fn test_apply_patch_hunks_accept_relative_and_absolute_paths() {
    for absolute in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("update.txt"), "old\n").unwrap();
        fs::write(dir.path().join("delete.txt"), "delete\n").unwrap();
        let raw = |name: &str| {
            if absolute {
                dir.path().join(name).display().to_string()
            } else {
                name.to_string()
            }
        };
        let applied = apply_patch(&patch(&format!("*** Add File: {}\n+added\n*** Update File: {}\n@@\n-old\n+new\n*** Delete File: {}", raw("add.txt"), raw("update.txt"), raw("delete.txt"))), Some(dir.path())).unwrap();
        assert_eq!(
            applied.summary,
            format!(
                "Success. Updated the following files:\nA {}\nM {}\nD {}\n",
                raw("add.txt"),
                raw("update.txt"),
                raw("delete.txt")
            )
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("add.txt")).unwrap(),
            "added\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("update.txt")).unwrap(),
            "new\n"
        );
        assert!(!dir.path().join("delete.txt").exists());
    }
}

#[test]
fn test_delete_file_hunk_removes_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("delete.txt");
    fs::write(&path, "gone\n").unwrap();
    let applied = apply_patch(
        &patch(&format!("*** Delete File: {}", path.display())),
        Some(dir.path()),
    )
    .unwrap();
    assert_eq!(
        applied.summary,
        format!(
            "Success. Updated the following files:\nD {}\n",
            path.display()
        )
    );
    assert!(!path.exists());
    assert_eq!(
        applied.changes[0].change,
        ChangeKind::Deleted {
            old: "gone\n".into()
        }
    );
}

#[test]
fn test_update_file_hunk_modifies_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("update.txt");
    fs::write(&path, "foo\nbar\n").unwrap();
    let applied = apply_patch(
        &patch(&format!(
            "*** Update File: {}\n@@\n foo\n-bar\n+baz",
            path.display()
        )),
        Some(dir.path()),
    )
    .unwrap();
    assert_eq!(
        applied.summary,
        format!(
            "Success. Updated the following files:\nM {}\n",
            path.display()
        )
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "foo\nbaz\n");
}

#[test]
fn test_update_file_hunk_can_move_file() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("old.txt");
    let dest = dir.path().join("new.txt");
    fs::write(&source, "foo\nbar\n").unwrap();
    let applied = apply_patch(
        &patch(&format!(
            "*** Update File: {}\n*** Move to: {}\n@@\n foo\n-bar\n+baz",
            source.display(),
            dest.display()
        )),
        Some(dir.path()),
    )
    .unwrap();
    assert_eq!(
        applied.summary,
        format!(
            "Success. Updated the following files:\nM {}\n",
            dest.display()
        )
    );
    assert!(!source.exists());
    assert_eq!(fs::read_to_string(&dest).unwrap(), "foo\nbaz\n");
    assert_eq!(
        applied.changes[0].change,
        ChangeKind::Moved {
            from: source,
            old: "foo\nbar\n".into(),
            new: "foo\nbaz\n".into(),
            overwritten: None
        }
    );
}

#[test]
fn test_multiple_update_chunks_apply_to_single_file() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("multi.txt"), "foo\nbar\nbaz\nqux\n").unwrap();
    let applied = apply_patch(
        &patch("*** Update File: multi.txt\n@@\n foo\n-bar\n+BAR\n@@\n baz\n-qux\n+QUX"),
        Some(dir.path()),
    )
    .unwrap();
    assert_eq!(
        applied.summary,
        "Success. Updated the following files:\nM multi.txt\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("multi.txt")).unwrap(),
        "foo\nBAR\nbaz\nQUX\n"
    );
}

#[test]
fn test_update_file_hunk_interleaved_changes() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("file"), "a\nb\nc\nd\ne\nf\n").unwrap();
    let applied = apply_patch(&patch("*** Update File: file\n@@\n a\n-b\n+B\n@@\n c\n d\n-e\n+E\n@@\n f\n+g\n*** End of File"), Some(dir.path())).unwrap();
    assert_eq!(
        applied.summary,
        "Success. Updated the following files:\nM file\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("file")).unwrap(),
        "a\nB\nc\nd\nE\nf\ng\n"
    );
}

#[test]
fn test_pure_addition_chunk_followed_by_removal() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("file"), "line1\nline2\nline3\n").unwrap();
    apply_patch(&patch("*** Update File: file\n@@\n+after-context\n+second-line\n@@\n line1\n-line2\n-line3\n+line2-replacement"), Some(dir.path())).unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("file")).unwrap(),
        "line1\nline2-replacement\nafter-context\nsecond-line\n"
    );
}

#[test]
fn test_update_line_with_unicode_dash() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("unicode.py"),
        "import asyncio  # local import – avoids top‑level dep\n",
    )
    .unwrap();
    let applied = apply_patch(&patch("*** Update File: unicode.py\n@@\n-import asyncio  # local import - avoids top-level dep\n+import asyncio  # HELLO"), Some(dir.path())).unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("unicode.py")).unwrap(),
        "import asyncio  # HELLO\n"
    );
    assert_eq!(
        applied.summary,
        "Success. Updated the following files:\nM unicode.py\n"
    );
}
