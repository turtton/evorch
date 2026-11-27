//! All global collection tests run under an injected pseudo-/var/tmp.
use super::super::managed::{self, Candidate, Format, Kind, Lease, MARKER, TTL};
use super::*;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, FileTimes},
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, PermissionsExt, symlink},
    },
    time::{SystemTime, UNIX_EPOCH},
};
fn modified(path: &Path, at: SystemTime) {
    File::open(path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(at))
        .unwrap();
}
fn old() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(100)
}
fn now() -> SystemTime {
    old() + TTL + Duration::from_secs(1)
}
fn fixture(base: &Path, kind: Kind, owner: Option<&Path>) -> PathBuf {
    let suffix = match kind {
        Kind::Fixed => {
            let digest: String = Sha256::digest(owner.unwrap().as_os_str().as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            format!("{}-{}", euid(), &digest[..8])
        }
        Kind::Ephemeral => format!("{}-a1B2c3", euid()),
        Kind::Standalone => "Zx9876".into(),
    };
    let path = base.join(format!("evo-{suffix}"));
    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    let fd = open(&path, FLAGS, Mode::empty()).unwrap();
    Format::new(kind, owner).write(&fd).unwrap();
    if let Some(owner) = owner {
        let digest: String = Sha256::digest(owner.as_os_str().as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        fs::write(path.join(IDENTITY_MARKER), digest).unwrap();
        fs::set_permissions(
            path.join(IDENTITY_MARKER),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    fs::write(path.join("payload"), "temporary").unwrap();
    modified(&path.join(MARKER), old());
    path
}
fn pace(shared: &Shared) -> Pace<'_> {
    Pace {
        shared,
        delay: Duration::ZERO,
        entries: 0,
        step: None,
        enumeration_error: None,
        foreign_inode: None,
        throttle: None,
    }
}
fn collect(base: &Path, at: SystemTime) {
    assert_eq!(
        scan_orphans(base, at, &mut pace(&Shared::default()), &mut HashSet::new()).unwrap(),
        Outcome::Complete
    );
}
#[test]
fn orphan_scratch_three_formats_and_internal_trash_are_collected_without_following_links() {
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("keep"), "keep").unwrap();
    for kind in [Kind::Fixed, Kind::Ephemeral, Kind::Standalone] {
        let owner = temp.path().join("missing-owner");
        let path = fixture(
            temp.path(),
            kind,
            (kind == Kind::Fixed).then_some(owner.as_path()),
        );
        fs::create_dir(path.join(".trash-interrupted")).unwrap();
        fs::write(path.join(".trash-interrupted/file"), "trash").unwrap();
        symlink(outside.path(), path.join("link")).unwrap();
        collect(temp.path(), now());
        assert!(!path.exists(), "{kind:?}");
    }
    assert_eq!(fs::read(outside.path().join("keep")).unwrap(), b"keep");
}
#[test]
fn orphan_scratch_unmarked_wrong_names_attachments_and_top_level_trash_are_retained() {
    let temp = tempfile::tempdir().unwrap();
    for name in [
        format!("evo-{}-a1B2c3", euid()),
        "evo-socket".into(),
        "evo-rv595".into(),
        format!("evo-{}-KGAMYl", euid()),
        format!("evo-{}-12345678", euid()),
        "evo-attachments-1000-12345678".into(),
        format!("evo-{}-a1B2c3", euid() + 1),
        ".trash-any".into(),
    ] {
        let path = temp.path().join(name);
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        fs::write(path.join("keep"), "keep").unwrap();
    }
    // Even a valid marker cannot turn an attachment or top-level trash into scratch.
    for name in [
        format!("evo-attachments-{}-{}", euid(), "a".repeat(64)),
        ".trash-marked".into(),
    ] {
        let path = temp.path().join(name);
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Format::new(Kind::Standalone, None)
            .write(&open(&path, FLAGS, Mode::empty()).unwrap())
            .unwrap();
        modified(&path.join(MARKER), old());
    }
    collect(temp.path(), now());
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 10);
}
#[test]
fn orphan_scratch_marker_validation_fails_closed_and_does_not_starve_valid_candidates() {
    for bad in [
        "missing",
        "corrupt",
        "version",
        "kind",
        "public-marker",
        "public-root",
        "hardlink",
        "symlink",
        "oversize",
        "owner",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let path = fixture(temp.path(), Kind::Ephemeral, None);
        let marker = path.join(MARKER);
        match bad {
            "missing" => fs::remove_file(&marker).unwrap(),
            "corrupt" => fs::write(&marker, b"broken").unwrap(),
            "version" => {
                fs::write(&marker, br#"{"version":2,"kind":"ephemeral","owner":null}"#).unwrap()
            }
            "kind" => fs::write(
                &marker,
                br#"{"version":1,"kind":"standalone","owner":null}"#,
            )
            .unwrap(),
            "public-marker" => {
                fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).unwrap()
            }
            "public-root" => fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap(),
            "hardlink" => fs::hard_link(&marker, temp.path().join("alias")).unwrap(),
            "symlink" => {
                fs::rename(&marker, temp.path().join("original")).unwrap();
                symlink(temp.path().join("original"), &marker).unwrap();
            }
            "oversize" => fs::write(&marker, vec![b' '; managed::LIMIT as usize + 1]).unwrap(),
            "owner" => {
                fs::write(&marker, br#"{"version":1,"kind":"ephemeral","owner":[47]}"#).unwrap()
            }
            _ => unreachable!(),
        }
        if bad != "missing" && bad != "symlink" {
            modified(&marker, old());
        }
        let valid = fixture(temp.path(), Kind::Standalone, None);
        collect(temp.path(), now());
        assert!(path.join("payload").exists(), "{bad}");
        assert!(!valid.exists(), "{bad}");
    }
}
#[test]
fn orphan_scratch_marker_round_trips_a_long_non_utf8_owner_within_its_bound() {
    use std::os::unix::ffi::OsStringExt;
    let temp = tempfile::tempdir().unwrap();
    let owner = PathBuf::from(std::ffi::OsString::from_vec([b'/', 0xff].repeat(2047)));
    Format::new(Kind::Fixed, Some(&owner))
        .write(&open(temp.path(), FLAGS, Mode::empty()).unwrap())
        .unwrap();
    let encoded = fs::read(temp.path().join(MARKER)).unwrap();
    assert!(encoded.len() > 8192);
    assert!(encoded.len() as u64 <= managed::LIMIT);
    let decoded: Format = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(serde_json::to_vec(&decoded).unwrap(), encoded);
}
#[test]
fn orphan_scratch_only_strictly_older_markers_expire() {
    let temp = tempfile::tempdir().unwrap();
    let path = fixture(temp.path(), Kind::Standalone, None);
    for at in [
        old() - Duration::from_secs(1),
        old(),
        old() + TTL - Duration::from_nanos(1),
        old() + TTL,
    ] {
        collect(temp.path(), at);
        assert!(path.join("payload").exists());
    }
    collect(temp.path(), now());
    assert!(!path.exists());
}
#[test]
fn orphan_scratch_fixed_requires_missing_owner_matching_digest_and_collision_marker() {
    for condition in [
        "exists",
        "lookup-error",
        "digest",
        "collision",
        "non-utf8-missing",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let owner = if condition == "non-utf8-missing" {
            use std::os::unix::ffi::OsStringExt;
            temp.path()
                .join(std::ffi::OsString::from_vec(b"owner-\xff".to_vec()))
        } else if condition == "lookup-error" {
            fs::write(temp.path().join("file"), "not a directory").unwrap();
            temp.path().join("file/owner")
        } else {
            temp.path().join("owner")
        };
        if condition == "exists" {
            fs::create_dir(&owner).unwrap();
        }
        let path = fixture(temp.path(), Kind::Fixed, Some(&owner));
        if condition == "collision" {
            fs::write(path.join(IDENTITY_MARKER), "different").unwrap();
        }
        let path = if condition == "digest" {
            let different = temp.path().join(format!("evo-{}-00000000", euid()));
            fs::rename(&path, &different).unwrap();
            different
        } else {
            path
        };
        collect(temp.path(), now());
        assert_eq!(
            path.exists(),
            condition != "non-utf8-missing",
            "{condition}"
        );
    }
}
#[test]
fn orphan_scratch_shared_lease_protects_root_and_exclusive_collection_never_blocks_users() {
    let temp = tempfile::tempdir().unwrap();
    let path = fixture(temp.path(), Kind::Standalone, None);
    let first = Lease::acquire(&path).unwrap();
    let second = Lease::acquire(&path).unwrap();
    collect(temp.path(), now());
    assert!(path.exists());
    drop(first);
    collect(temp.path(), now());
    assert!(path.exists());
    drop(second);
    let parent = open(temp.path(), FLAGS, Mode::empty()).unwrap();
    let name = CString::new(path.file_name().unwrap().as_bytes()).unwrap();
    let collector = Candidate::open(&parent, &name, now()).unwrap().unwrap();
    assert!(Lease::acquire(&path).is_err());
    drop(collector);
    collect(temp.path(), now());
    assert!(!path.exists());
}
#[test]
fn orphan_scratch_root_marker_and_symlink_replacements_are_not_deleted() {
    for replaced in ["root", "marker", "symlink"] {
        let temp = tempfile::tempdir().unwrap();
        let path = fixture(temp.path(), Kind::Standalone, None);
        let parent = open(temp.path(), FLAGS, Mode::empty()).unwrap();
        let name = CString::new(path.file_name().unwrap().as_bytes()).unwrap();
        let candidate = Candidate::open(&parent, &name, now()).unwrap().unwrap();
        if replaced == "marker" {
            fs::rename(path.join(MARKER), path.join(".old-marker")).unwrap();
            Format::new(Kind::Standalone, None)
                .write(&candidate.fd)
                .unwrap();
            modified(&path.join(MARKER), old());
        } else {
            let aside = temp.path().join("aside");
            fs::rename(&path, &aside).unwrap();
            if replaced == "symlink" {
                symlink(&aside, &path).unwrap();
            } else {
                fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
                fs::write(path.join("keep"), "keep").unwrap();
            }
        }
        assert!(
            remove_tree_managed(
                &parent,
                &name,
                Some(candidate.inode()),
                &mut pace(&Shared::default()),
                Some(&candidate)
            )
            .is_err()
        );
        assert!(path.exists());
        if replaced == "marker" {
            assert!(path.join("payload").exists());
        }
    }
}
#[test]
fn orphan_scratch_foreign_child_is_retained_and_marker_survives_incomplete_deletion() {
    // Inject fstat's foreign ownership at a specific inode; no chown privilege needed.
    for directory in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let path = fixture(temp.path(), Kind::Standalone, None);
        let child = path.join("foreign");
        if directory {
            fs::create_dir(&child).unwrap();
            fs::write(child.join("keep"), "keep").unwrap();
        } else {
            fs::write(&child, "keep").unwrap();
        }
        let s = statat(rustix::fs::CWD, &child, AtFlags::SYMLINK_NOFOLLOW).unwrap();
        let parent = open(temp.path(), FLAGS, Mode::empty()).unwrap();
        let name = CString::new(path.file_name().unwrap().as_bytes()).unwrap();
        let candidate = Candidate::open(&parent, &name, now()).unwrap().unwrap();
        let shared = Shared::default();
        let mut pace = pace(&shared);
        pace.foreign_inode = Some(identity(&s));
        assert!(
            remove_tree_managed(
                &parent,
                &name,
                Some(candidate.inode()),
                &mut pace,
                Some(&candidate)
            )
            .is_err()
        );
        assert!(child.exists());
        assert!(path.join(MARKER).exists());
        if directory {
            assert!(child.join("keep").exists());
        }
    }
}
#[test]
fn orphan_scratch_fixed_owner_recreated_after_inspection_is_retained() {
    let temp = tempfile::tempdir().unwrap();
    let owner = temp.path().join("owner");
    let path = fixture(temp.path(), Kind::Fixed, Some(&owner));
    let parent = open(temp.path(), FLAGS, Mode::empty()).unwrap();
    let name = CString::new(path.file_name().unwrap().as_bytes()).unwrap();
    let candidate = Candidate::open(&parent, &name, now()).unwrap().unwrap();
    fs::create_dir(&owner).unwrap();
    assert!(
        remove_tree_managed(
            &parent,
            &name,
            Some(candidate.inode()),
            &mut pace(&Shared::default()),
            Some(&candidate)
        )
        .is_err()
    );
    assert!(path.join("payload").exists());
    assert!(path.join(MARKER).exists());
}
#[test]
fn orphan_scratch_stop_and_resume_retain_marker_and_owner_until_completion() {
    let temp = tempfile::tempdir().unwrap();
    let owner = temp.path().join("missing");
    let path = fixture(temp.path(), Kind::Fixed, Some(&owner));
    let shared = Shared::default();
    let step = || {
        // Enumeration also has entry boundaries; stop only after a real unlink.
        if !path.join("payload").exists() {
            lock(&shared.work).stop = true;
        }
    };
    let mut paced = pace(&shared);
    paced.step = Some(&step);
    assert_eq!(
        scan_orphans(temp.path(), now(), &mut paced, &mut HashSet::new()).unwrap(),
        Outcome::Stopped
    );
    assert!(
        !path.join("payload").exists(),
        "interruption was after a real unlink"
    );
    assert!(path.join(MARKER).exists());
    assert!(path.join(IDENTITY_MARKER).exists());
    lock(&shared.work).stop = false;
    collect(temp.path(), now());
    assert!(!path.exists());
}
#[test]
fn orphan_scratch_worker_queues_while_stopped_and_rescans_after_resume() {
    let temp = tempfile::tempdir().unwrap();
    let path = fixture(temp.path(), Kind::Standalone, None);
    let (tx, rx) = std::sync::mpsc::channel();
    let mut service = CleanupService::new(Duration::ZERO);
    service.events = Some(tx);
    service.stop().unwrap();
    service.request_orphans(temp.path(), now()).unwrap();
    assert!(path.exists());
    service.resume().unwrap();
    while !matches!(rx.recv().unwrap(), TestEvent::Pass) {}
    service.stop().unwrap();
    assert!(!path.exists());
}
#[test]
fn orphan_scratch_fixed_marker_is_new_only_and_hidden_from_normal_sweeps() {
    let temp = tempfile::tempdir().unwrap();
    for existing in [true, false] {
        let parent = temp.path().join(if existing { "existing" } else { "new" });
        if existing {
            // Model a valid old root independently of the runner's umask.
            fs::DirBuilder::new().mode(0o700).create(&parent).unwrap();
        }
        let mut root = ScratchRoot::at(parent.clone()).identified("full-digest");
        root.managed = Some(Format::new(Kind::Fixed, Some(&temp.path().join("owner"))));
        root.prepare("thread").unwrap();
        for name in root.names().unwrap() {
            root.remove_bounded(&name, usize::MAX).unwrap();
        }
        assert_eq!(parent.join(MARKER).exists(), !existing);
        assert!(parent.join(IDENTITY_MARKER).exists());
    }
    let chosen = ScratchRoot::at(temp.path().join("chosen"));
    chosen.prepare("thread").unwrap();
    assert!(!chosen.parent().join(MARKER).exists());
}
#[test]
fn orphan_scratch_uid_checks_and_cached_lease_replacement_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let path = fixture(temp.path(), Kind::Standalone, None);
    let mut s = fstat(open(&path, FLAGS, Mode::empty()).unwrap()).unwrap();
    assert!(managed::private(&s));
    s.st_uid = euid() + 1;
    assert!(!managed::private(&s));
    let lease = Lease::acquire(&path).unwrap();
    fs::rename(&path, temp.path().join("old")).unwrap();
    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    assert!(lease.validate(&path).is_err());
}

#[test]
fn orphan_scratch_marked_name_uid_mismatch_and_top_level_symlink_are_retained() {
    for linked in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let path = fixture(temp.path(), Kind::Ephemeral, None);
        let wrong = temp.path().join(format!("evo-{}-a1B2c3", euid() + 1));
        fs::rename(&path, &wrong).unwrap();
        if linked {
            symlink(&wrong, &path).unwrap();
        }
        collect(temp.path(), now());
        assert!(wrong.join("payload").exists());
        if linked {
            assert!(path.symlink_metadata().unwrap().is_symlink());
        }
    }
}
#[test]
fn orphan_scratch_marker_replaced_during_unlink_keeps_root_discoverable() {
    let temp = tempfile::tempdir().unwrap();
    let path = fixture(temp.path(), Kind::Standalone, None);
    let parent = open(temp.path(), FLAGS, Mode::empty()).unwrap();
    let name = CString::new(path.file_name().unwrap().as_bytes()).unwrap();
    let candidate = Candidate::open(&parent, &name, now()).unwrap().unwrap();
    let shared = Shared::default();
    let replaced = std::sync::atomic::AtomicBool::new(false);
    let step = || {
        if !path.join("payload").exists()
            && !replaced.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            fs::remove_file(path.join(MARKER)).unwrap();
            Format::new(Kind::Standalone, None)
                .write(&candidate.fd)
                .unwrap();
        }
    };
    let mut paced = pace(&shared);
    paced.step = Some(&step);
    assert!(
        remove_tree_managed(
            &parent,
            &name,
            Some(candidate.inode()),
            &mut paced,
            Some(&candidate)
        )
        .is_err()
    );
    assert!(replaced.load(std::sync::atomic::Ordering::SeqCst));
    assert!(path.join(MARKER).exists());
}
