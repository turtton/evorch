use super::*;

#[test]
fn exclusive_sweep_notifies_each_restored_run_only_for_actually_detached_names() {
    for thread in ["plain", "thread/with spaces/日本語"] {
        let base = tempfile::tempdir().unwrap();
        let scratch = Arc::new(ThreadScratch::at(base.path().join("scratch")));
        let key = ScratchKey::Thread(thread.into());
        let path = scratch.prepare(&key).unwrap();
        std::fs::write(path.join("before"), "keep").unwrap();
        let protected = HashSet::from([thread.to_owned()]);
        scratch.sweep(&HashSet::new(), true); // No exclusive proof / planned startup.
        scratch.sweep(&protected, false); // A recovery graph retains this thread.
        assert!(path.join("before").exists());
        assert!(!scratch.take_emptied_notice(&key, RunId::new(1), true));
        scratch.sweep(&HashSet::new(), false);
        assert!(!path.exists());
        // The runtime receives this same Arc. Parent and child do not consume
        // each other's cleanup notification; a new run never gets a loss notice.
        let runtime_scratch = scratch.clone();
        for run in [RunId::new(1), RunId::new(2)] {
            assert!(runtime_scratch.take_emptied_notice(&key, run, true));
            assert!(!runtime_scratch.take_emptied_notice(&key, run, true));
        }
        assert!(!scratch.take_emptied_notice(&key, RunId::new(3), false));
        assert!(!scratch.take_emptied_notice(&key, RunId::new(3), true));
        scratch.sweep(&HashSet::new(), false); // Absent namespace is not a reset.
        assert!(!scratch.take_emptied_notice(&key, RunId::new(1), true));
        let missing = ScratchKey::Thread("never-existed".into());
        assert!(!scratch.take_emptied_notice(&missing, RunId::new(4), true));
    }
}

#[cfg(unix)]
#[test]
fn failed_sweep_does_not_claim_that_files_were_emptied() {
    use std::os::unix::fs::PermissionsExt;
    let base = tempfile::tempdir().unwrap();
    let scratch = ThreadScratch::at(base.path().join("scratch"));
    let key = ScratchKey::Thread("thread".into());
    let path = scratch.prepare(&key).unwrap();
    std::fs::write(path.join("keep"), "keep").unwrap();
    std::fs::set_permissions(scratch.parent(), std::fs::Permissions::from_mode(0o500)).unwrap();
    scratch.sweep(&HashSet::new(), false);
    std::fs::set_permissions(scratch.parent(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(std::fs::read(path.join("keep")).unwrap(), b"keep");
    assert!(!scratch.take_emptied_notice(&key, RunId::new(1), true));
}

#[test]
fn archive_binding_gate_allows_unbound_without_creating_binding_and_rejects_mismatch() {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("state");
    std::fs::create_dir(&root).unwrap();
    let config = storage::StorageConfig {
        db_path: base.path().join("events.db"),
        ..Default::default()
    };
    drop(storage::Database::open(&config).unwrap());
    let leases = Arc::new(
        storage::WriterLeases::acquire(&config.db_path, &root, std::time::Duration::ZERO).unwrap(),
    );
    let context = archive::ScratchAuthority {
        leases,
        root: root.clone(),
        sidebar: base.path().join("sidebar.json"),
    };
    assert!(context.validate(&config.db_path).unwrap());
    assert!(!root.join(".evorch-retention-binding.json").exists());
    storage::Database::open(&config)
        .unwrap()
        .retention_identity()
        .unwrap();
    std::fs::write(
        root.join(".evorch-retention-binding.json"),
        r#"{"db_id":"another-db","db_path":"/another/events.db","bound_at_ns":1}"#,
    )
    .unwrap();
    assert!(!context.validate(&config.db_path).unwrap());
}
