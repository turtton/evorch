//! Moves must not unlink the content destination they just updated.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use apply_patch::{ChangeKind, apply_patch};

fn patch(source: &str, destination: &str) -> String {
    format!(
        "*** Begin Patch\n*** Update File: {source}\n*** Move to: {destination}\n@@\n-old\n+new\n*** End Patch"
    )
}

fn layout() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("real")).unwrap();
    fs::write(root.join("real/source"), "old\n").unwrap();
    symlink("real/source", root.join("alias")).unwrap();
    symlink("alias", root.join("chain")).unwrap();
    symlink("real", root.join("dir-alias")).unwrap();
    symlink("source", root.join("real/back-link")).unwrap();
    dir
}

fn assert_move_is_an_update(source: &str, destination: &str) {
    let dir = layout();
    let root = dir.path();
    let applied = apply_patch(&patch(source, destination), Some(root)).unwrap();
    assert_eq!(
        fs::read_to_string(root.join("real/source")).unwrap(),
        "new\n"
    );
    assert_eq!(fs::read_to_string(root.join(source)).unwrap(), "new\n");
    assert_eq!(fs::read_to_string(root.join(destination)).unwrap(), "new\n");
    for link in ["alias", "chain", "dir-alias", "real/back-link"] {
        assert!(fs::symlink_metadata(root.join(link)).unwrap().is_symlink());
    }
    assert_eq!(applied.changes.len(), 1);
    assert_eq!(
        applied.changes[0].path,
        root.canonicalize().unwrap().join("real/source")
    );
    assert_eq!(applied.changes[0].raw, Path::new(destination));
    assert_eq!(
        applied.changes[0].change,
        ChangeKind::Updated {
            old: "old\n".into(),
            new: "new\n".into(),
        }
    );
    assert_eq!(
        applied.summary,
        format!("Success. Updated the following files:\nM {destination}\n")
    );
}

#[test]
fn move_to_a_symlink_back_to_source_is_an_update() {
    assert_move_is_an_update("real/source", "alias");
}

#[test]
fn move_to_a_symlink_chain_back_to_source_is_an_update() {
    assert_move_is_an_update("real/source", "chain");
}

#[test]
fn move_via_directory_aliases_to_a_symlink_back_to_source_is_an_update() {
    assert_move_is_an_update("dir-alias/source", "dir-alias/back-link");
}

#[test]
fn move_to_a_directory_alias_of_the_same_entry_stays_an_update() {
    assert_move_is_an_update("real/source", "dir-alias/source");
}

#[test]
fn move_to_a_symlink_chain_through_the_source_link_is_an_update() {
    assert_move_is_an_update("alias", "chain");
}

#[test]
fn move_between_distinct_symlinks_to_one_referent_only_unlinks_the_source_link() {
    let dir = layout();
    let root = dir.path();
    let applied = apply_patch(&patch("alias", "real/back-link"), Some(root)).unwrap();
    assert!(fs::symlink_metadata(root.join("alias")).is_err());
    assert!(
        fs::symlink_metadata(root.join("real/back-link"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(root.join("real/source")).unwrap(),
        "new\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("real/back-link")).unwrap(),
        "new\n"
    );
    assert_eq!(applied.changes.len(), 1);
    assert_eq!(
        applied.changes[0].path,
        root.canonicalize().unwrap().join("real/source")
    );
    assert_eq!(applied.changes[0].raw, Path::new("real/back-link"));
    assert_eq!(
        applied.changes[0].change,
        ChangeKind::Moved {
            from: root.canonicalize().unwrap().join("alias"),
            old: "old\n".into(),
            new: "new\n".into(),
            overwritten: Some("old\n".into()),
        }
    );
}
