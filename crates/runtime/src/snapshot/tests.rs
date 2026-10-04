use super::*;
use std::fs;
use std::process::Command;

#[test]
fn benchmark_snapshot_roundtrip_restores_ignored_files_and_removes_future_artifacts() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("trial");
    fs::create_dir(&root).unwrap();
    fs::write(root.join(".gitignore"), "ignored*\n").unwrap();
    fs::write(root.join("ignored-config"), "checkpoint").unwrap();
    fs::create_dir_all(root.join("original-empty/nested")).unwrap();
    let directory = temp.path().join("snapshots");
    let mut store = SnapshotStore::open(&root, &directory).unwrap();
    let snapshot = store.capture_all().unwrap();
    let persisted = snapshot.as_str().to_owned();
    fs::remove_dir_all(root.join("original-empty")).unwrap();
    fs::create_dir_all(root.join("future-outcome/empty")).unwrap();
    let directory_only = store.capture_all().unwrap();
    assert_ne!(
        snapshot, directory_only,
        "directory names are snapshot state"
    );
    assert_eq!(store.diff(&snapshot, &directory_only).unwrap(), "");
    fs::write(root.join("ignored-config"), "future changed config").unwrap();
    fs::write(root.join("ignored-future-evidence"), "future outcome").unwrap();
    drop(store);
    let mut reopened = SnapshotStore::open(&root, &directory).unwrap();
    reopened
        .restore_all(&SnapshotId::from_hex(persisted).unwrap())
        .unwrap();
    assert_eq!(
        fs::read_to_string(root.join("ignored-config")).unwrap(),
        "checkpoint"
    );
    assert!(!root.join("ignored-future-evidence").exists());
    assert!(root.join("original-empty/nested").is_dir());
    assert!(!root.join("future-outcome").exists());
    reopened.restore_all(&directory_only).unwrap();
    assert!(!root.join("original-empty").exists());
    assert!(root.join("future-outcome/empty").is_dir());
    assert!(SnapshotId::from_hex("--all".into()).is_err());
}

#[cfg(unix)]
#[test]
fn benchmark_snapshot_preserves_crlf_bytes_and_unix_modes_despite_git_defaults() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("trial");
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o750)).unwrap();
    fs::set_permissions(root.join("nested"), fs::Permissions::from_mode(0o750)).unwrap();
    let file = root.join("nested/input.txt");
    let bytes = b"one\r\ntwo\r\n";
    fs::write(&file, bytes).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    let mut store = SnapshotStore::open(&root, &temp.path().join("snapshots")).unwrap();
    // Host defaults that would normalize captured bytes are overridden per
    // benchmark command without modifying the user's or snapshot Git config.
    let attributes = temp.path().join("host-attributes");
    fs::write(&attributes, "* text eol=lf\n").unwrap();
    store.git(&["config", "core.autocrlf", "input"]).unwrap();
    store
        .git(&[
            "config",
            "core.attributesFile",
            attributes.to_str().unwrap(),
        ])
        .unwrap();
    let snapshot = store.capture_all().unwrap();
    let tree = store.file_tree(&snapshot).unwrap();
    let stored = store
        .git(&["show", &format!("{}:nested/input.txt", tree.as_str())])
        .unwrap();
    assert_eq!(
        stored.stdout, bytes,
        "snapshot blob must preserve exact bytes"
    );
    fs::set_permissions(&file, fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(root.join("nested"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let changed_modes = store.capture_all().unwrap();
    assert_ne!(
        snapshot, changed_modes,
        "mode-only changes are snapshot state"
    );
    store.restore_all(&snapshot).unwrap();
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o7777,
        0o644
    );
    assert_eq!(
        fs::metadata(root.join("nested"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o750
    );
    assert_eq!(
        fs::metadata(&root).unwrap().permissions().mode() & 0o7777,
        0o750
    );
    fs::write(&file, "changed\n").unwrap();
    store.restore_all(&snapshot).unwrap();
    assert_eq!(fs::read(&file).unwrap(), bytes);
    assert_eq!(
        store.git(&["config", "core.autocrlf"]).unwrap().stdout,
        b"input\n"
    );
}

#[cfg(unix)]
#[test]
fn benchmark_snapshot_rejects_special_files_and_attributes_before_index_or_workspace_changes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("trial");
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::write(root.join("file"), "original").unwrap();
    let mut store = SnapshotStore::open(&root, &temp.path().join("snapshots")).unwrap();
    let snapshot = store.capture_all().unwrap();
    fs::write(root.join("file"), "future").unwrap();
    for name in ["pipe", "socket", ".gitattributes", "nested/.gitattributes"] {
        let path = root.join(name);
        let _listener = match name {
            "pipe" => {
                assert!(
                    Command::new("mkfifo")
                        .arg(&path)
                        .status()
                        .unwrap()
                        .success()
                );
                None
            }
            "socket" => Some(std::os::unix::net::UnixListener::bind(&path).unwrap()),
            _ => {
                fs::write(&path, "* text eol=lf\n").unwrap();
                None
            }
        };
        let index = fs::read(store.git_dir.join("index")).unwrap();
        let capture_error = store.capture_all().unwrap_err().to_string();
        let restore_error = store.restore_all(&snapshot).unwrap_err().to_string();
        assert!(capture_error.contains(name), "{capture_error}");
        assert!(restore_error.contains(name), "{restore_error}");
        assert_eq!(fs::read(store.git_dir.join("index")).unwrap(), index);
        assert_eq!(fs::read_to_string(root.join("file")).unwrap(), "future");
        fs::remove_file(path).unwrap();
    }
    store.restore_all(&snapshot).unwrap();
    assert_eq!(fs::read_to_string(root.join("file")).unwrap(), "original");
}

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
