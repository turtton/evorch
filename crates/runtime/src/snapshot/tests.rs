use super::*;
use std::fs;
use std::process::Command;

#[cfg(unix)]
#[test]
fn store_rejects_repository_symlink() {
    // Given: a previously initialized store whose repository was replaced by a symlink.
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("project");
    fs::create_dir(&root).expect("root");
    let directory = temp.path().join("snapshots");
    SnapshotStore::open(&root, &directory).expect("store");
    let repository = directory.join("repository");
    fs::rename(&repository, temp.path().join("user-git")).expect("move");
    std::os::unix::fs::symlink(temp.path().join("user-git"), &repository).expect("symlink");
    // When / Then: reopening refuses redirected repository metadata.
    assert!(SnapshotStore::open(&root, &directory).is_err());
}

#[test]
fn capture_excludes_worktree_git_pointer_and_restores_deleted_files() {
    // Given: a linked-worktree style metadata pointer and a captured file.
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("project");
    fs::create_dir(&root).expect("root");
    fs::write(root.join(".git"), "gitdir: /private/user/git\n").expect("pointer");
    fs::write(root.join("file"), "original").expect("file");
    let directory = temp.path().join("snapshots");
    let mut store = SnapshotStore::open(&root, &directory).expect("store");
    let before = store.capture().expect("before");
    fs::remove_file(root.join("file")).expect("delete");
    let after = store.capture().expect("after");
    // When: reopen the store and restore the earlier tree.
    let mut reopened = SnapshotStore::open(&root, &directory).expect("reopen");
    reopened.restore(&before).expect("restore");
    // Then: the file is restored without capturing or modifying the git pointer.
    assert_eq!(
        fs::read_to_string(root.join("file")).expect("file"),
        "original"
    );
    assert_eq!(
        fs::read_to_string(root.join(".git")).expect("pointer"),
        "gitdir: /private/user/git\n"
    );
    assert!(
        !reopened
            .diff(&before, &after)
            .expect("diff")
            .contains("private/user")
    );
}

#[test]
fn restore_roundtrip_preserves_user_index_and_ignored_files() {
    // Given: staged user changes, an ignored file and an independent store.
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("project");
    fs::create_dir(&root).expect("project");
    assert!(
        Command::new("git")
            .arg("init")
            .arg(&root)
            .status()
            .expect("git")
            .success()
    );
    fs::write(root.join(".gitignore"), "secret\n").expect("ignore");
    fs::write(root.join("file"), "before\n").expect("file");
    fs::write(root.join("secret"), "keep").expect("secret");
    assert!(
        Command::new("git")
            .current_dir(&root)
            .args(["add", "."])
            .status()
            .expect("git")
            .success()
    );
    let index = fs::read(root.join(".git/index")).expect("index");
    let mut store = SnapshotStore::open(&root, &temp.path().join("snapshots")).expect("store");
    let before = store.capture().expect("before");
    fs::write(root.join("file"), "after\n").expect("edit");
    fs::write(root.join("new"), "new\n").expect("new");
    let after = store.capture().expect("after");
    assert!(
        store
            .diff(&before, &after)
            .expect("diff")
            .contains("+after")
    );

    // When: restore and then redo the captured state.
    store.restore(&before).expect("undo");
    assert_eq!(
        fs::read_to_string(root.join("file")).expect("file"),
        "before\n"
    );
    assert!(!root.join("new").exists());
    store.restore(&after).expect("redo");

    // Then: tracked/new files roundtrip, private data and the user's index do not change.
    assert_eq!(
        fs::read_to_string(root.join("file")).expect("file"),
        "after\n"
    );
    assert_eq!(fs::read_to_string(root.join("new")).expect("new"), "new\n");
    assert_eq!(
        fs::read_to_string(root.join("secret")).expect("secret"),
        "keep"
    );
    assert_eq!(fs::read(root.join(".git/index")).expect("index"), index);
}

#[test]
fn store_rejects_metadata_inside_workspace() {
    // Given: a workspace root.
    let temp = tempfile::tempdir().expect("tempdir");
    // When: attempting to keep metadata within the captured tree.
    let result = SnapshotStore::open(temp.path(), &temp.path().join("snapshots"));
    // Then: reject recursive/self-capturing stores.
    assert!(result.is_err());
}

#[test]
fn thread_history_keeps_redo_until_a_new_checkpoint() {
    // Given: an independent workspace and two thread histories.
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("project");
    fs::create_dir(&root).expect("root");
    fs::write(root.join("file"), "one").expect("file");
    let mut store = SnapshotStore::open(&root, &temp.path().join("snapshots")).expect("store");
    let mut history = SnapshotHistory::default();
    history.checkpoint("a", &mut store).expect("checkpoint");
    fs::write(root.join("file"), "two").expect("edit");
    // When: undo in one thread and attempt redo in another.
    assert!(history.undo("a", &mut store).expect("undo"));
    assert!(!history.redo("b", &mut store).expect("other thread"));
    assert!(history.redo("a", &mut store).expect("redo"));
    // Then: redo restores the changed file and a new checkpoint clears redo.
    assert_eq!(fs::read_to_string(root.join("file")).expect("file"), "two");
    assert!(history.undo("a", &mut store).expect("undo"));
    history.checkpoint("a", &mut store).expect("checkpoint");
    assert!(!history.redo("a", &mut store).expect("redo cleared"));
}
