use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use apply_patch::{ChangeKind, ErrorClass, apply_patch, plan};

fn patch(body: &str) -> String {
    format!("*** Begin Patch\n{body}\n*** End Patch")
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let rel = path.strip_prefix(root).unwrap().to_path_buf();
            if path.is_dir() {
                out.insert(rel, None);
                visit(root, &path, out);
            } else {
                out.insert(rel, Some(fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}

#[test]
fn planning_is_read_only_and_combines_sequential_updates() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("file"), "old\n").unwrap();
    let before = snapshot(dir.path());
    let plan = plan(&patch("*** Update File: file\n@@\n-old\n+middle\n*** Update File: ./file\n@@\n-middle\n+final"), Some(dir.path())).unwrap();
    assert_eq!(snapshot(dir.path()), before);
    let applied = plan.commit().unwrap();
    assert_eq!(applied.changes.len(), 1);
    assert_eq!(
        applied.changes[0].change,
        ChangeKind::Updated {
            old: "old\n".into(),
            new: "final\n".into()
        }
    );
    assert_eq!(
        applied.summary,
        "Success. Updated the following files:\nM file\nM ./file\n"
    );
}

#[test]
fn sequential_add_update_delete_readd_and_move_update() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("deleted"), "old\n").unwrap();
    fs::write(dir.path().join("source"), "source\n").unwrap();
    fs::write(dir.path().join("dest"), "overwritten\n").unwrap();
    let applied = apply_patch(&patch("*** Add File: added\n+first\n*** Update File: added\n@@\n-first\n+last\n*** Delete File: deleted\n*** Add File: deleted\n+reborn\n*** Update File: source\n*** Move to: dest\n@@\n-source\n+middle\n*** Update File: dest\n@@\n-middle\n+final"), Some(dir.path())).unwrap();
    assert_eq!(applied.changes.len(), 3);
    assert!(applied.changes.iter().any(|c| c.change
        == ChangeKind::Added {
            content: "last\n".into(),
            overwritten: None
        }));
    assert!(applied.changes.iter().any(|c| c.change
        == ChangeKind::Added {
            content: "reborn\n".into(),
            overwritten: Some("old\n".into())
        }));
    assert!(applied.changes.iter().any(|c| c.change
        == ChangeKind::Moved {
            from: dir.path().join("source"),
            old: "source\n".into(),
            new: "final\n".into(),
            overwritten: Some("overwritten\n".into())
        }));
    assert!(!dir.path().join("source").exists());
    assert_eq!(
        fs::read_to_string(dir.path().join("dest")).unwrap(),
        "final\n"
    );
}

#[test]
fn same_resolved_move_is_an_update() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("file"), "old\n").unwrap();
    let applied = apply_patch(
        &patch("*** Update File: file\n*** Move to: ./file\n@@\n-old\n+new"),
        Some(dir.path()),
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("file")).unwrap(),
        "new\n"
    );
    assert_eq!(
        applied.changes[0].change,
        ChangeKind::Updated {
            old: "old\n".into(),
            new: "new\n".into()
        }
    );
    assert_eq!(applied.changes[0].raw, Path::new("./file"));
    assert_eq!(
        applied.summary,
        "Success. Updated the following files:\nM ./file\n"
    );
}

#[test]
fn all_planning_failures_leave_the_entire_tree_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("file"), "old\n").unwrap();
    fs::create_dir(dir.path().join("directory")).unwrap();
    for (body, class) in [
        (
            "*** Add File: new/sub\n+hello\n*** Update File: file\n@@\n-missing\n+new",
            ErrorClass::Match,
        ),
        (
            "*** Add File: new/sub\n+hello\n*** Update File: missing\n@@\n-old\n+new",
            ErrorClass::Io,
        ),
        (
            "*** Add File: new/sub\n+hello\n*** Delete File: directory",
            ErrorClass::Io,
        ),
        (
            "*** Add File: new/sub\n+hello\n*** Delete File: missing",
            ErrorClass::Io,
        ),
        (
            "*** Add File: new/sub\n+hello\n*** Update File: file\n@@ context\n-old\n+new",
            ErrorClass::Match,
        ),
        (
            "*** Add File: new/sub\n+hello\n*** Add File: ../escape\n+x",
            ErrorClass::Path,
        ),
        (
            "*** Add File: new/sub\n+hello\n*** Update File: file\n@@",
            ErrorClass::Parse,
        ),
    ] {
        let before = snapshot(dir.path());
        assert_eq!(
            plan(&patch(body), Some(dir.path())).unwrap_err().class(),
            class
        );
        assert_eq!(snapshot(dir.path()), before, "{body}");
    }
}

#[test]
fn commit_failure_reports_only_persisted_files_and_leaks_no_temps() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(
        &patch("*** Add File: a\n+done\n*** Add File: z\n+blocked\n*** Delete File: old"),
        Some(dir.path()),
    );
    // Delete must exist during planning.
    assert!(plan.is_err());
    fs::write(dir.path().join("old"), "retained\n").unwrap();
    let plan = apply_patch::plan(
        &patch("*** Add File: a\n+done\n*** Add File: z\n+blocked\n*** Delete File: old"),
        Some(dir.path()),
    )
    .unwrap();
    // A deterministic persist failure, independent of uid and permission bypass.
    fs::create_dir(dir.path().join("z")).unwrap();
    let failure = plan.commit().unwrap_err();
    assert_eq!(failure.error.class(), ErrorClass::Io);
    assert_eq!(failure.committed.len(), 1);
    assert_eq!(failure.committed[0].raw, Path::new("a"));
    assert!(
        failure
            .to_string()
            .ends_with("\nApplied before the failure:\nA a\n")
    );
    assert_eq!(
        snapshot(dir.path()).keys().cloned().collect::<Vec<_>>(),
        vec![PathBuf::from("a"), PathBuf::from("old"), PathBuf::from("z")]
    );
}

#[test]
fn summary_keeps_raw_paths_and_upstream_kind_order() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("delete"), "gone\n").unwrap();
    fs::write(dir.path().join("update"), "old\n").unwrap();
    let applied = apply_patch(&patch("*** Delete File: delete\n*** Update File: ./update\n@@\n-old\n+new\n*** Add File: nested/add\n+x"), Some(dir.path())).unwrap();
    assert_eq!(
        applied.summary,
        "Success. Updated the following files:\nA nested/add\nM ./update\nD delete\n"
    );
    assert_eq!(
        plan("*** Begin Patch\n*** End Patch", Some(dir.path()))
            .unwrap_err()
            .to_string(),
        "No files were modified."
    );
    assert_eq!(
        plan("bad", None).unwrap_err().to_string(),
        "Invalid patch: The first line of the patch must be '*** Begin Patch'"
    );
}

#[cfg(unix)]
#[test]
fn executable_mode_survives_add_update_and_move() {
    use std::os::unix::fs::PermissionsExt;
    for body in [
        "*** Add File: file\n+new",
        "*** Update File: file\n@@\n-old\n+new",
        "*** Update File: file\n*** Move to: dest\n@@\n-old\n+new",
    ] {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file"), "old\n").unwrap();
        fs::set_permissions(dir.path().join("file"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.path().join("dest"), "other\n").unwrap();
        apply_patch(&patch(body), Some(dir.path())).unwrap();
        let dest = if body.contains("Move to") {
            "dest"
        } else {
            "file"
        };
        assert_eq!(
            fs::metadata(dir.path().join(dest))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert!(
            snapshot(dir.path())
                .keys()
                .all(|p| !p.to_string_lossy().starts_with(".tmp"))
        );
    }
}

#[test]
fn net_noops_do_not_write_or_emit_changes() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("file"), "old\n").unwrap();
    let before = snapshot(dir.path());
    let applied = apply_patch(&patch("*** Add File: nested/new\n+intermediate\n*** Delete File: nested/new\n*** Update File: file\n@@\n-old\n+middle\n*** Update File: file\n@@\n-middle\n+old"), Some(dir.path())).unwrap();
    assert!(applied.changes.is_empty());
    assert_eq!(snapshot(dir.path()), before);
    assert_eq!(
        applied.summary,
        "Success. Updated the following files:\nA nested/new\nM file\nM file\nD nested/new\n"
    );
}

#[test]
fn move_deletion_failure_reports_only_the_written_destination() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("source"), "old\n").unwrap();
    let plan = plan(
        &patch("*** Update File: source\n*** Move to: nested/dest\n@@\n-old\n+new"),
        Some(dir.path()),
    )
    .unwrap();
    // Simulate a failure between read-only planning and the removal phase.
    fs::remove_file(dir.path().join("source")).unwrap();
    fs::create_dir(dir.path().join("source")).unwrap();
    let failure = plan.commit().unwrap_err();
    assert!(failure.to_string().starts_with(&format!(
        "Failed to remove original {}:",
        dir.path().join("source").display()
    )));
    assert_eq!(failure.committed.len(), 1);
    assert_eq!(
        failure.committed[0].change,
        ChangeKind::Added {
            content: "new\n".into(),
            overwritten: None
        }
    );
    assert!(
        failure
            .to_string()
            .ends_with("\nApplied before the failure:\nA nested/dest\n")
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("nested/dest")).unwrap(),
        "new\n"
    );
    assert_eq!(snapshot(dir.path()).len(), 3);
}

#[test]
fn commit_failure_before_first_persist_has_no_applied_suffix() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(&patch("*** Add File: parent/child\n+x"), Some(dir.path())).unwrap();
    fs::write(dir.path().join("parent"), "blocker").unwrap();
    let before = snapshot(dir.path());
    let failure = plan.commit().unwrap_err();
    assert!(failure.committed.is_empty());
    assert_eq!(failure.to_string(), failure.error.to_string());
    // std reports a non-directory path component as a write failure; upstream
    // also only retries parent creation for NotFound, not NotADirectory.
    assert!(failure.to_string().starts_with("Failed to write file "));
    assert_eq!(snapshot(dir.path()), before);
}

#[test]
fn error_messages_match_upstream_and_relative_paths_need_root() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("file"), "old\n").unwrap();
    let path = dir.path().join("file");
    assert_eq!(
        plan(
            &patch("*** Update File: file\n@@ context\n-old\n+new"),
            Some(dir.path())
        )
        .unwrap_err()
        .to_string(),
        format!("Failed to find context 'context' in {}", path.display())
    );
    assert_eq!(
        plan(
            &patch("*** Update File: file\n@@\n-missing\n+new"),
            Some(dir.path())
        )
        .unwrap_err()
        .to_string(),
        format!(
            "Failed to find expected lines in {}:\nmissing",
            path.display()
        )
    );
    assert_eq!(
        plan(&patch("*** Update File: file\n@@"), Some(dir.path()))
            .unwrap_err()
            .to_string(),
        "Invalid patch hunk on line 4: Update hunk does not contain any lines"
    );
    assert_eq!(
        plan(&patch("*** Add File: file\n+x"), None)
            .unwrap_err()
            .class(),
        ErrorClass::Path
    );
    let applied = apply_patch(
        &patch(&format!(
            "*** Add File: {}\n+x",
            dir.path().join("absolute").display()
        )),
        None,
    )
    .unwrap();
    assert_eq!(applied.changes[0].path, dir.path().join("absolute"));
}

#[cfg(unix)]
#[test]
fn symlink_alias_updates_preserve_both_changes() {
    assert_alias_updates("two");
}

#[cfg(unix)]
#[test]
fn symlink_alias_matching_reads_prior_virtual_updates() {
    assert_alias_updates("ONE");
}

#[cfg(unix)]
fn assert_alias_updates(second_old: &str) {
    use std::os::unix::fs::symlink;
    for final_link in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("real")).unwrap();
        fs::write(dir.path().join("real/f.txt"), "one\ntwo\n").unwrap();
        let alias = if final_link {
            symlink("real/f.txt", dir.path().join("alias")).unwrap();
            "alias"
        } else {
            symlink("real", dir.path().join("alias")).unwrap();
            "alias/f.txt"
        };
        let applied = apply_patch(
                &patch(&format!(
                    "*** Update File: real/f.txt\n@@\n-one\n+ONE\n*** Update File: {alias}\n@@\n-{second_old}\n+TWO"
                )),
                Some(dir.path()),
            )
            .unwrap();
        let expected = if second_old == "two" {
            "ONE\nTWO\n"
        } else {
            "TWO\ntwo\n"
        };
        assert_eq!(
            fs::read_to_string(dir.path().join("real/f.txt")).unwrap(),
            expected
        );
        assert_eq!(applied.changes.len(), 1);
        assert_eq!(
            applied.changes[0].path,
            dir.path().canonicalize().unwrap().join("real/f.txt")
        );
        assert_eq!(applied.changes[0].raw, Path::new(alias));
        assert_eq!(
            applied.changes[0].change,
            ChangeKind::Updated {
                old: "one\ntwo\n".into(),
                new: expected.into()
            }
        );
        assert_eq!(
            applied.summary,
            format!("Success. Updated the following files:\nM real/f.txt\nM {alias}\n")
        );
    }
}

#[cfg(unix)]
#[test]
fn updating_then_deleting_a_symlink_keeps_the_referent_update() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("target"), "old\n").unwrap();
    std::os::unix::fs::symlink("target", dir.path().join("link")).unwrap();
    let applied = apply_patch(
        &patch("*** Update File: link\n@@\n-old\n+new\n*** Delete File: link"),
        Some(dir.path()),
    )
    .unwrap();
    assert!(fs::symlink_metadata(dir.path().join("link")).is_err());
    assert_eq!(
        fs::read_to_string(dir.path().join("target")).unwrap(),
        "new\n"
    );
    assert_eq!(applied.changes.len(), 2);
}

#[cfg(unix)]
#[test]
fn moving_a_symlink_reads_virtual_contents_and_only_unlinks_the_source_entry() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("target"), "old\n").unwrap();
    std::os::unix::fs::symlink("target", dir.path().join("link")).unwrap();
    let applied = apply_patch(
        &patch("*** Update File: target\n@@\n-old\n+new\n*** Update File: link\n*** Move to: dest\n@@\n-new\n+moved"),
        Some(dir.path()),
    ).unwrap();
    assert!(fs::symlink_metadata(dir.path().join("link")).is_err());
    assert_eq!(
        fs::read_to_string(dir.path().join("target")).unwrap(),
        "new\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("dest")).unwrap(),
        "moved\n"
    );
    assert_eq!(applied.changes.len(), 2);
    assert!(applied.changes.iter().any(|change| matches!(&change.change, ChangeKind::Moved { from, old, new, .. } if from == &dir.path().join("link") && old == "old\n" && new == "moved\n")));
}

#[cfg(unix)]
#[test]
fn deleting_then_readding_a_symlink_replaces_only_the_entry() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("target"), "old\n").unwrap();
    std::os::unix::fs::symlink("target", dir.path().join("link")).unwrap();
    let applied = apply_patch(
        &patch("*** Delete File: link\n*** Add File: link\n+old\n*** Update File: link\n@@\n-old\n+new"),
        Some(dir.path()),
    ).unwrap();
    assert!(
        fs::symlink_metadata(dir.path().join("link"))
            .unwrap()
            .is_file()
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("target")).unwrap(),
        "old\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("link")).unwrap(),
        "new\n"
    );
    assert_eq!(applied.changes.len(), 1);
    assert_eq!(applied.changes[0].path, dir.path().join("link"));
}

#[cfg(unix)]
#[test]
fn adding_through_a_dangling_link_then_deleting_it_removes_only_the_link() {
    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("target", dir.path().join("link")).unwrap();
    let applied = apply_patch(
        &patch("*** Add File: link\n+created\n*** Delete File: link"),
        Some(dir.path()),
    )
    .unwrap();
    assert!(fs::symlink_metadata(dir.path().join("link")).is_err());
    assert_eq!(
        fs::read_to_string(dir.path().join("target")).unwrap(),
        "created\n"
    );
    assert_eq!(applied.changes.len(), 2);
}

#[cfg(unix)]
#[test]
fn update_follows_symlinks_without_replacing_the_link() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("target"), "old\n").unwrap();
    symlink("target", dir.path().join("link")).unwrap();
    apply_patch(
        &patch("*** Update File: link\n@@\n-old\n+new"),
        Some(dir.path()),
    )
    .unwrap();
    assert!(
        fs::symlink_metadata(dir.path().join("link"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("target")).unwrap(),
        "new\n"
    );
    symlink("missing", dir.path().join("dangling")).unwrap();
    apply_patch(&patch("*** Add File: dangling\n+created"), Some(dir.path())).unwrap();
    assert!(
        fs::symlink_metadata(dir.path().join("dangling"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("missing")).unwrap(),
        "created\n"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn update_follows_the_std_symlink_depth_limit() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("target"), "old\n").unwrap();
    let mut target = PathBuf::from("target");
    // Linux follows up to 40 symlinks. The atomic-write destination lookup
    // must not reject a chain std::fs successfully read during planning.
    for index in 0..40 {
        let link = PathBuf::from(format!("link{index}"));
        symlink(&target, dir.path().join(&link)).unwrap();
        target = link;
    }
    assert_eq!(
        fs::read_to_string(dir.path().join(&target)).unwrap(),
        "old\n"
    );
    apply_patch(
        &patch("*** Update File: link39\n@@\n-old\n+new"),
        Some(dir.path()),
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("target")).unwrap(),
        "new\n"
    );
    assert!(
        fs::symlink_metadata(dir.path().join(target))
            .unwrap()
            .is_symlink()
    );
}

#[test]
fn matching_and_line_endings_are_upstream_compatible() {
    for (original, body, expected) in [
        (
            "same\nother\nsame\n",
            "@@\n-same\n+last\n*** End of File",
            "same\nother\nlast\n",
        ),
        ("old\n", "@@\n-old\n-\n+new\n+", "new\n"),
        (
            "head\r\n  keep\nold\rtail\r\n",
            "@@\n head\n keep\n-old\n+new\n tail",
            "head\r\n  keep\nnew\r\ntail\r\n",
        ),
        (
            "old\r\n",
            "@@\n-old\n+new\n+inserted",
            "new\r\ninserted\r\n",
        ),
        ("old\r", "@@\n-old\n+new", "new\r"),
        ("old", "@@\n-old\n+new", "new\n"),
        ("head\nlast", "@@\n+inserted", "head\nlast\ninserted\n"),
        (
            "head\r\nlast",
            "@@\n+inserted",
            "head\r\nlast\r\ninserted\r\n",
        ),
        ("", "@@\n+inserted", "inserted\n"),
        ("old\n", "@@\n-old", ""),
        (
            "line1\nline2\nline3\n",
            "@@\n+after-context\n+second-line\n@@\n line1\n-line2\n-line3\n+line2-replacement",
            "line1\nline2-replacement\nafter-context\nsecond-line\n",
        ),
        (
            "keep\r\nold\nkeep\rtail\n",
            "@@ keep\n-old\n+new\n keep\n tail",
            "keep\r\nnew\r\nkeep\rtail\n",
        ),
        (
            "head\r\n\r\nold\rtail\n",
            "@@\n head\n \n-old\n+new\n tail",
            "head\r\n\r\nnew\r\ntail\n",
        ),
        ("old\n", "-old\n+new", "new\n"),
        ("head\n\nold\n", "@@\n head\n \n-old\n+new", "head\n\nnew\n"),
        (
            "x – ‘quote’\u{a0}ok\n",
            "@@\n-x - 'quote' ok\n+new",
            "new\n",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file"), original).unwrap();
        apply_patch(
            &patch(&format!("*** Update File: file\n{body}")),
            Some(dir.path()),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("file")).unwrap(),
            expected,
            "{body}"
        );
    }
}
