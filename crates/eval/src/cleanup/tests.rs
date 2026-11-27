use super::*;
use crate::output::{BindingSummary, RunEntry, StartSummary};
use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, symlink};

fn fixture(root: &Path, finished: bool, lock: bool) {
    std::fs::create_dir_all(root).unwrap();
    let manifest = Manifest {
        thread_id: "fixture".into(),
        source_db: "source.db".into(),
        start: StartSummary {
            project_root: "project".into(),
            repository: "project".into(),
            repo_prefix: String::new(),
            head_commit: "head".into(),
            head_ref: None,
            diff_bytes: 0,
            untracked_entries: 0,
            captured_at_ns: 0,
        },
        original_turns: vec![],
        problem: None,
        sample: None,
        verify: None,
        candidate_overlay: None,
        swapped_roles: vec![],
        common_overlay: None,
        changes: Default::default(),
        pins: None,
        configurations: vec!["baseline".into()],
        repeat: 1,
        simulator: BindingSummary {
            source: "fixture".into(),
            binding: Default::default(),
        },
        budget: Default::default(),
        started_at: "fixture".into(),
        finished_at: finished.then(|| "finished".into()),
        // Hostile manifest paths must not direct deletion.
        runs: vec![RunEntry {
            label: "baseline".into(),
            index: 1,
            dir: "../../outside".into(),
            nonce: "fixture".into(),
            observed: None,
            started_at: "fixture".into(),
            finished_at: None,
            status: None,
        }],
    };
    crate::output::write_json(&root.join(MANIFEST_FILE), &manifest).unwrap();
    if lock {
        File::create(root.join(LOCK_FILE)).unwrap();
    }
    write(&root.join("preflight-workspace/data"), b"abc");
    write(&root.join("runs/baseline-1/workspace/data"), b"12345");
}
fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let meta = std::fs::symlink_metadata(path).unwrap();
        let bytes = if meta.is_symlink() {
            std::fs::read_link(path)
                .unwrap()
                .into_os_string()
                .into_vec()
        } else if meta.is_file() {
            std::fs::read(path).unwrap()
        } else {
            vec![]
        };
        out.insert(path.strip_prefix(root).unwrap().into(), bytes);
        if meta.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                visit(root, &entry.unwrap().path(), out);
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}
fn one(root: &Path, options: Options) -> Report {
    clean(&[root.into()], None, options)
}
fn skipped(report: &Report, reason: &str) -> bool {
    report
        .entries
        .iter()
        .any(|entry| matches!(&entry.action, Action::Skip(found) if found.contains(reason)))
}

#[test]
fn dry_run_sizes_and_selective_idempotent_removal() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("eval");
    let a = home.join("new");
    let b = home.join("legacy");
    fixture(&a, false, true);
    fixture(&b, true, false);
    let outside = temp.path().join("outside");
    write(&outside.join("sentinel"), b"safe");
    let link_target = outside.as_os_str().as_encoded_bytes().len() as u64;
    symlink(&outside, a.join("preflight-workspace/link")).unwrap();
    symlink("missing", a.join("preflight-workspace/dangling")).unwrap();
    let raw_run = a.join("runs").join(OsString::from_vec(vec![0xff]));
    write(
        &raw_run
            .join("workspace")
            .join(OsString::from_vec(vec![0xfe])),
        b"xy",
    );
    write(&b.join("runs/candidate-1/workspace/file"), b"sixsix");
    for root in [&a, &b] {
        for name in [
            "preflight.json",
            "report.json",
            "report.md",
            "judgments/item",
            ".groups/item",
            "other/nested/workspace/safe",
            "runs/baseline-1/evorch.db",
            "runs/baseline-1/evorch.db-wal",
            "runs/baseline-1/evorch.db-shm",
            "runs/baseline-1/diff.patch",
            "runs/baseline-1/conversation.json",
            "runs/baseline-1/outcome.json",
        ] {
            write(&root.join(name), b"keep");
        }
    }
    let before = snapshot(temp.path());
    let dry = clean(
        &[],
        Some(&home),
        Options {
            dry_run: true,
            ..Default::default()
        },
    );
    assert!(!dry.has_failures(), "{dry:?}");
    assert_eq!(snapshot(temp.path()), before);
    assert_eq!(dry.entries.len(), 6);
    let sizes = |report: &Report| {
        report
            .entries
            .iter()
            .map(|entry| match entry.size {
                Size::Bytes(bytes) => bytes,
                _ => panic!("{entry:?}"),
            })
            .sum::<u64>()
    };
    assert_eq!(sizes(&dry), 3 + 5 + 2 + 3 + 5 + 6 + link_target + 7);
    assert!(
        dry.entries
            .iter()
            .all(|entry| entry.action == Action::WouldRemove)
    );
    let removed = clean(&[], Some(&home), Options::default());
    assert!(!removed.has_failures(), "{removed:?}");
    assert_eq!(sizes(&removed), sizes(&dry));
    let after = snapshot(temp.path());
    for (path, bytes) in before {
        let is_workspace = removed
            .entries
            .iter()
            .any(|entry| temp.path().join(&path).starts_with(&entry.path));
        if !is_workspace {
            assert_eq!(after.get(&path), Some(&bytes), "{}", path.display());
        }
    }
    assert_eq!(std::fs::read(outside.join("sentinel")).unwrap(), b"safe");
    assert!(
        clean(&[], Some(&home), Options::default())
            .entries
            .is_empty()
    );
}

#[test]
fn locks_and_legacy_decision_table() {
    let temp = tempfile::tempdir().unwrap();
    let new = temp.path().join("new");
    fixture(&new, false, true);
    let lock = File::open(new.join(LOCK_FILE)).unwrap();
    lock.lock().unwrap();
    let inode = lock.metadata().unwrap().ino();
    let before = snapshot(&new);
    for include_unfinished in [false, true] {
        let report = one(
            &new,
            Options {
                include_unfinished,
                ..Default::default()
            },
        );
        assert!(skipped(&report, "busy"), "{report:?}");
        assert_eq!(snapshot(&new), before);
    }
    drop(lock);
    let report = one(&new, Options::default());
    assert_eq!(report.entries.len(), 2);
    assert!(
        report
            .entries
            .iter()
            .all(|entry| entry.action == Action::Removed)
    );
    assert_eq!(std::fs::metadata(new.join(LOCK_FILE)).unwrap().ino(), inode);
    for finished in [false, true] {
        let root = temp
            .path()
            .join(if finished { "finished" } else { "unfinished" });
        fixture(&root, finished, false);
        let before = snapshot(&root);
        let report = one(&root, Options::default());
        if finished {
            assert!(
                report
                    .entries
                    .iter()
                    .all(|entry| entry.action == Action::Removed)
            );
        } else {
            assert!(skipped(&report, "unfinished-legacy"));
            assert_eq!(snapshot(&root), before);
            let report = one(
                &root,
                Options {
                    include_unfinished: true,
                    ..Default::default()
                },
            );
            assert_eq!(report.unfinished_legacy, std::slice::from_ref(&root));
            assert!(
                report
                    .entries
                    .iter()
                    .all(|entry| entry.action == Action::Removed)
            );
        }
        assert!(!root.join(LOCK_FILE).exists());
    }
    let invalid = temp.path().join("invalid");
    fixture(&invalid, true, true);
    std::fs::write(invalid.join(MANIFEST_FILE), b"{}").unwrap();
    let before = snapshot(&invalid);
    assert!(skipped(
        &one(
            &invalid,
            Options {
                include_unfinished: true,
                ..Default::default()
            }
        ),
        "invalid manifest"
    ));
    assert_eq!(snapshot(&invalid), before);
    std::fs::remove_file(invalid.join(MANIFEST_FILE)).unwrap();
    assert!(skipped(
        &one(&invalid, Options::default()),
        "not-evaluation"
    ));
}

#[test]
fn no_follow_at_every_discovered_level_but_explicit_root_is_trusted() {
    let temp = tempfile::tempdir().unwrap();
    let outside = temp.path().join("outside");
    fixture(&outside, true, true);
    write(&outside.join("sentinel"), b"safe");
    let before = snapshot(&outside);
    for component in [
        "preflight-workspace",
        "runs",
        "runs/baseline-1",
        "runs/baseline-1/workspace",
    ] {
        let home = temp.path().join(component.replace('/', "-"));
        let root = home.join("evaluation");
        fixture(&root, true, true);
        let target = root.join(component);
        std::fs::remove_dir_all(&target).unwrap();
        symlink(&outside, &target).unwrap();
        let report = clean(&[], Some(&home), Options::default());
        assert!(skipped(&report, "symlink"), "{report:?}");
        assert!(std::fs::symlink_metadata(&target).unwrap().is_symlink());
        assert_eq!(snapshot(&outside), before);
    }
    let home = temp.path().join("discovery");
    std::fs::create_dir(&home).unwrap();
    symlink(&outside, home.join("linked-evaluation")).unwrap();
    symlink(temp.path().join("absent"), home.join("dangling")).unwrap();
    let report = clean(&[], Some(&home), Options::default());
    assert_eq!(report.entries.len(), 2);
    assert!(
        report
            .entries
            .iter()
            .all(|entry| entry.action == Action::Skip("symlink".into()))
    );
    assert_eq!(snapshot(&outside), before);
    let report = one(&home.join("linked-evaluation"), Options::default());
    assert!(!report.has_failures());
    assert!(!outside.join("preflight-workspace").exists());
    assert_eq!(std::fs::read(outside.join("sentinel")).unwrap(), b"safe");
}

#[test]
fn workspace_replacement_does_not_redirect_traversal_or_stop_other_workspaces() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("eval");
    fixture(&root, true, true);
    let outside = temp.path().join("outside");
    write(&outside.join("sentinel"), b"safe");
    let link = outside.clone();
    HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |path| {
            std::fs::rename(path, path.with_file_name("moved-workspace")).unwrap();
            symlink(&link, path).unwrap();
        }))
    });
    let report = one(&root, Options::default());
    assert!(report.has_failures(), "{report:?}");
    assert!(matches!(report.entries[0].size, Size::Unknown(_)));
    assert!(!root.join("runs/baseline-1/workspace").exists());
    assert_eq!(std::fs::read(outside.join("sentinel")).unwrap(), b"safe");
    assert!(
        std::fs::symlink_metadata(root.join("preflight-workspace"))
            .unwrap()
            .is_symlink()
    );
}

#[test]
fn lock_io_failures_never_fall_back_to_unlocked_deletion() {
    let temp = tempfile::tempdir().unwrap();
    fixture(temp.path(), true, false);
    std::fs::create_dir(temp.path().join(LOCK_FILE)).unwrap();
    let before = snapshot(temp.path());
    let report = one(temp.path(), Options::default());
    assert!(report.has_failures());
    assert_eq!(snapshot(temp.path()), before);
    assert!(RunnerGuard::acquire(temp.path()).is_err());
}

#[test]
fn a_different_device_is_refused_before_any_traversal() {
    let temp = tempfile::tempdir().unwrap();
    fixture(temp.path(), true, true);
    let before = snapshot(temp.path());
    let mut root = Dir::root(temp.path()).unwrap_or_else(|problem| panic!("{}", problem.reason));
    // Model the stat result at a filesystem boundary without privileged mounts.
    root.stat.st_dev = root.stat.st_dev.wrapping_add(1);
    let mut report = Report::default();
    workspaces(&root, false, true, &mut report);
    assert_eq!(report.entries.len(), 2);
    assert!(report.entries.iter().all(|entry| matches!(&entry.action, Action::Failed(reason) if reason.contains("different filesystem"))));
    assert_eq!(snapshot(temp.path()), before);
}
