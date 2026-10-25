use std::fs;
use std::path::{Path, PathBuf};

use apply_patch::{ChangeKind, apply_patch, plan};

fn patch(body: &str) -> String {
    format!("*** Begin Patch\n{body}\n*** End Patch")
}

#[test]
fn move_chains_report_the_original_source_and_final_destination() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("source"), "old\n").unwrap();
    fs::write(dir.path().join("middle"), "middle overwritten\n").unwrap();
    fs::write(dir.path().join("dest"), "dest overwritten\n").unwrap();
    let applied = apply_patch(&patch("*** Update File: source\n*** Move to: middle\n@@\n-old\n+intermediate\n*** Update File: middle\n*** Move to: dest\n@@\n-intermediate\n+final"), Some(dir.path())).unwrap();
    assert_eq!(applied.changes.len(), 2);
    assert!(
        applied
            .changes
            .iter()
            .any(|c| c.path == dir.path().join("dest")
                && c.change
                    == ChangeKind::Moved {
                        from: dir.path().join("source"),
                        old: "old\n".into(),
                        new: "final\n".into(),
                        overwritten: Some("dest overwritten\n".into()),
                    })
    );
    assert!(
        applied
            .changes
            .iter()
            .any(|c| c.path == dir.path().join("middle")
                && c.change
                    == ChangeKind::Deleted {
                        old: "middle overwritten\n".into()
                    })
    );
    assert!(!dir.path().join("source").exists());
    assert!(!dir.path().join("middle").exists());
    assert_eq!(
        fs::read_to_string(dir.path().join("dest")).unwrap(),
        "final\n"
    );
}

#[test]
fn newly_added_source_then_move_is_one_final_add() {
    let dir = tempfile::tempdir().unwrap();
    let applied = apply_patch(&patch("*** Add File: source\n+old\n*** Update File: source\n*** Move to: dest\n@@\n-old\n+new"), Some(dir.path())).unwrap();
    assert_eq!(applied.changes.len(), 1);
    assert_eq!(applied.changes[0].path, dir.path().join("dest"));
    assert_eq!(
        applied.changes[0].change,
        ChangeKind::Added {
            content: "new\n".into(),
            overwritten: None
        }
    );
    assert!(!dir.path().join("source").exists());
}

#[test]
fn unchanged_move_destination_only_reports_source_deletion() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("source"), "same\n").unwrap();
    fs::write(dir.path().join("dest"), "same\n").unwrap();
    let applied = apply_patch(
        &patch("*** Update File: source\n*** Move to: dest\n@@\n same"),
        Some(dir.path()),
    )
    .unwrap();
    assert_eq!(applied.changes.len(), 1);
    assert_eq!(applied.changes[0].path, dir.path().join("source"));
    assert_eq!(
        applied.changes[0].change,
        ChangeKind::Deleted {
            old: "same\n".into()
        }
    );
    assert_eq!(
        applied.summary,
        "Success. Updated the following files:\nM dest\n"
    );
    assert!(!dir.path().join("source").exists());
    assert_eq!(
        fs::read_to_string(dir.path().join("dest")).unwrap(),
        "same\n"
    );
}

#[test]
fn move_then_readd_source_reports_two_final_changes() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("source"), "old\n").unwrap();
    let applied = apply_patch(&patch("*** Update File: source\n*** Move to: dest\n@@\n-old\n+moved\n*** Add File: source\n+recreated"), Some(dir.path())).unwrap();
    assert_eq!(applied.changes.len(), 2);
    assert!(
        applied
            .changes
            .iter()
            .any(|c| c.path == dir.path().join("source")
                && c.change
                    == ChangeKind::Added {
                        content: "recreated\n".into(),
                        overwritten: Some("old\n".into())
                    })
    );
    assert!(
        applied
            .changes
            .iter()
            .any(|c| c.path == dir.path().join("dest")
                && c.change
                    == ChangeKind::Added {
                        content: "moved\n".into(),
                        overwritten: None
                    })
    );
}

#[test]
fn write_phase_failure_does_not_remove_any_sources() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("source"), "old\n").unwrap();
    fs::write(dir.path().join("a"), "overwritten\n").unwrap();
    let plan = plan(
        &patch(
            "*** Update File: source\n*** Move to: a\n@@\n-old\n+new\n*** Add File: z\n+blocked",
        ),
        Some(dir.path()),
    )
    .unwrap();
    fs::create_dir(dir.path().join("z")).unwrap();
    let failure = plan.commit().unwrap_err();
    assert_eq!(failure.committed.len(), 1);
    assert_eq!(
        failure.committed[0].change,
        ChangeKind::Updated {
            old: "overwritten\n".into(),
            new: "new\n".into()
        }
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("source")).unwrap(),
        "old\n"
    );
    assert_eq!(fs::read_to_string(dir.path().join("a")).unwrap(), "new\n");
    assert!(
        failure
            .to_string()
            .ends_with("\nApplied before the failure:\nM a\n")
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 3);
}

#[cfg(unix)]
struct RestorePermissions(PathBuf, fs::Permissions);

#[cfg(unix)]
impl Drop for RestorePermissions {
    fn drop(&mut self) {
        fs::set_permissions(&self.0, self.1.clone()).unwrap();
    }
}

#[cfg(unix)]
fn read_only(dir: &Path) -> RestorePermissions {
    use std::os::unix::fs::PermissionsExt;
    let original = fs::metadata(dir).unwrap().permissions();
    fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
    RestorePermissions(dir.to_path_buf(), original)
}

#[cfg(unix)]
#[test]
fn readonly_directory_reports_aggregated_committed_prefix() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("z")).unwrap();
    fs::write(dir.path().join("old"), "retained\n").unwrap();
    let plan = plan(&patch("*** Add File: a\n+first\n*** Update File: a\n@@\n-first\n+final\n*** Add File: z/blocked\n+blocked\n*** Delete File: old"), Some(dir.path())).unwrap();
    let _restore = read_only(&dir.path().join("z"));
    // Capability-based probe rather than assuming a uid cannot bypass mode bits.
    match tempfile::NamedTempFile::new_in(dir.path().join("z")) {
        Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied),
        Ok(_) => {
            eprintln!(
                "directory mode is bypassed in this environment; deterministic failure tests still run"
            );
            return;
        }
    }
    let failure = plan.commit().unwrap_err();
    assert_eq!(failure.committed.len(), 1);
    assert_eq!(failure.committed[0].raw, Path::new("a"));
    assert_eq!(
        failure.committed[0].change,
        ChangeKind::Added {
            content: "final\n".into(),
            overwritten: None
        }
    );
    assert!(failure.to_string().starts_with(&format!(
        "Failed to write file {}:",
        dir.path().join("z/blocked").display()
    )));
    assert!(
        failure
            .to_string()
            .ends_with("\nApplied before the failure:\nA a\n")
    );
    assert_eq!(fs::read_to_string(dir.path().join("a")).unwrap(), "final\n");
    assert_eq!(
        fs::read_to_string(dir.path().join("old")).unwrap(),
        "retained\n"
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 3);
    assert_eq!(fs::read_dir(dir.path().join("z")).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn readonly_source_directory_keeps_written_move_dest_unmoved() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/file"), "old\n").unwrap();
    let plan = plan(
        &patch("*** Update File: src/file\n*** Move to: dest\n@@\n-old\n+new"),
        Some(dir.path()),
    )
    .unwrap();
    let _restore = read_only(&dir.path().join("src"));
    match tempfile::NamedTempFile::new_in(dir.path().join("src")) {
        Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied),
        Ok(_) => {
            eprintln!(
                "directory mode is bypassed in this environment; deterministic failure tests still run"
            );
            return;
        }
    }
    let failure = plan.commit().unwrap_err();
    assert_eq!(failure.committed.len(), 1);
    assert_eq!(
        failure.committed[0].change,
        ChangeKind::Added {
            content: "new\n".into(),
            overwritten: None
        }
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("src/file")).unwrap(),
        "old\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("dest")).unwrap(),
        "new\n"
    );
    assert!(
        failure
            .to_string()
            .ends_with("\nApplied before the failure:\nA dest\n")
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    assert_eq!(fs::read_dir(dir.path().join("src")).unwrap().count(), 1);
}
