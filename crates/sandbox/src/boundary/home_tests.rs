use super::*;
use std::fs;
use std::os::unix::fs::symlink;

fn boundary(home: &Path) -> SandboxBoundary {
    SandboxBoundary {
        host_home: canonical_home(Some(home)),
        cargo_home: Some(home.join(".cargo")),
        rustup_home: Some(home.join(".rustup")),
        ..SandboxBoundary::new(home.join("project"))
    }
}

fn aliases(boundary: &SandboxBoundary) -> Vec<(PathBuf, PathBuf, PathAccess)> {
    boundary
        .mount_plan(None, false)
        .mounts
        .into_iter()
        .filter(|m| m.home_alias)
        .map(|m| (m.host, m.sandbox, m.access))
        .collect()
}

#[test]
fn home_requires_an_absolute_existing_directory_other_than_root() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("file");
    fs::write(&file, "").unwrap();
    for home in [
        None,
        Some(Path::new("")),
        Some(Path::new("relative")),
        Some(Path::new("/")),
        Some(file.as_path()),
        Some(temp.path().join("missing").as_path()),
    ] {
        assert_eq!(canonical_home(home), None, "{home:?}");
    }
    let link = temp.path().join("link");
    let real = temp.path().join("real");
    fs::create_dir(&real).unwrap();
    symlink(&real, &link).unwrap();
    assert_eq!(canonical_home(Some(&link)), Some(real));
}

#[test]
fn only_strict_canonical_home_descendants_are_mirrored() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("user");
    let child = home.join(".cache/tool/data");
    let sibling = temp.path().join("userX/data");
    fs::create_dir_all(&child).unwrap();
    fs::create_dir_all(&sibling).unwrap();
    symlink(&sibling, home.join("out")).unwrap();
    symlink(&child, temp.path().join("in")).unwrap();
    symlink(&home, temp.path().join("home-link")).unwrap();
    let declared = vec![
        home.clone(),
        temp.path().into(),
        sibling.clone(),
        home.join("out").canonicalize().unwrap(),
        temp.path().join("in").canonicalize().unwrap(),
    ];
    for access in [PathAccess::ReadOnly, PathAccess::ReadWrite] {
        let mut boundary = boundary(&temp.path().join("home-link"))
            .without_tool_caches()
            .with_allowed(LivePaths::new(declared.clone()), access);
        // A workspace outside HOME avoids a deliberate final workspace alias.
        boundary.workspace_root = temp.path().join("project");
        assert_eq!(
            aliases(&boundary),
            [(
                child.clone(),
                PathBuf::from("/tmp/home/.cache/tool/data"),
                access
            )]
        );
        let plan = boundary.mount_plan(None, false);
        for original in &declared {
            assert!(
                plan.mounts
                    .iter()
                    .any(|m| m.host == *original && m.sandbox == *original && m.access == access)
            );
        }
        boundary.host_home = None;
        assert!(aliases(&boundary).is_empty());
    }
}

#[test]
fn parent_first_mirrors_and_originals_use_one_live_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().canonicalize().unwrap();
    let paths = LivePaths::new(vec![home.join("a/deep"), home.join("a"), home.join("a")]);
    let boundary = boundary(&home).with_allowed(paths.clone(), PathAccess::ReadWrite);
    let plan = boundary.mount_plan(None, false);
    let mirrors: Vec<_> = plan.mounts.iter().filter(|m| m.home_alias).collect();
    assert_eq!(mirrors.len(), 2);
    assert_eq!(mirrors[0].sandbox, Path::new("/tmp/home/a"));
    assert_eq!(mirrors[1].sandbox, Path::new("/tmp/home/a/deep"));
    paths.set(vec![home.join("other")]);
    assert!(plan.mounts.iter().any(|m| m.host == home.join("a")));
    let next = boundary.mount_plan(None, false);
    assert!(!next.mounts.iter().any(|m| m.host == home.join("a")));
    let other: Vec<_> = next
        .mounts
        .iter()
        .filter(|m| m.host == home.join("other"))
        .collect();
    assert_eq!(other.len(), 2);
    assert_eq!(other[0].access, other[1].access);
}

#[test]
fn cache_overlap_and_runtime_overrides_agree_at_originals_and_aliases() {
    use PathAccess::{ReadOnly, ReadWrite};
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().canonicalize().unwrap();
    for relative in [
        ".cargo",
        ".cargo/targets",
        ".cargo/registry/src/deep",
        ".cargo/registry/src/deep/locked",
        ".rustup",
        ".rustup/toolchains/stable",
        ".cargo/registry/src/deep/project",
    ] {
        fs::create_dir_all(home.join(relative)).unwrap();
    }
    for allowed in [
        ".cargo",
        ".cargo/targets",
        ".cargo/registry/src/deep",
        ".rustup",
        ".rustup/toolchains/stable",
    ] {
        for access in [ReadOnly, ReadWrite] {
            let allowed = home.join(allowed);
            let mut boundary =
                boundary(&home).with_allowed(LivePaths::new(vec![allowed.clone()]), access);
            boundary.workspace_root = home.join(".cargo/registry/src/deep/project");
            boundary = boundary.read_only(home.join(".cargo/registry/src/deep/locked"));
            let plan = boundary.mount_plan(None, true);
            for relative in [
                ".cargo/targets",
                ".cargo/registry/src/deep",
                ".cargo/registry/src/deep/locked",
                ".cargo/registry/src/deep/project",
                ".rustup/toolchains/stable",
            ] {
                let host = home.join(relative);
                if !host.starts_with(&allowed) {
                    continue;
                }
                let alias = Path::new("/tmp/home").join(relative);
                let effective = plan
                    .mounts
                    .iter()
                    .rev()
                    .find(|m| alias.starts_with(&m.sandbox))
                    .map(|m| m.access);
                assert_eq!(effective, boundary.access(&host), "{host:?}");
            }
            for cache in plan.mounts.iter().filter(|m| m.cache) {
                let mirror = Path::new("/tmp/home").join(allowed.strip_prefix(&home).unwrap());
                assert!(!cache.sandbox.starts_with(&mirror));
                if mirror.starts_with(&cache.sandbox) {
                    let position =
                        |path: &Path| plan.mounts.iter().position(|m| m.sandbox == path).unwrap();
                    assert!(position(&cache.sandbox) < position(&mirror));
                }
            }
        }
    }
    // No host permission for a shell-only alias.
    let b = boundary(&home).with_allowed(LivePaths::new(vec![home.join(".cargo")]), ReadWrite);
    assert_eq!(b.access(Path::new("/tmp/home/.cargo/targets/x")), None);
}

#[test]
fn parent_ro_and_later_rw_only_project_the_allowed_intersection() {
    use PathAccess::{ReadOnly, ReadWrite};
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().canonicalize().unwrap();
    let root = home.join("shared");
    let allowed = root.join("data");
    let child = allowed.join("child");
    fs::create_dir_all(&child).unwrap();
    let mut b = boundary(&home)
        .with_allowed(LivePaths::new(vec![allowed.clone()]), ReadWrite)
        .read_only(&root)
        .read_write(&child);
    b.workspace_root = root.join("project");
    let aliases = aliases(&b);
    assert_eq!(
        aliases
            .iter()
            .map(|(_, p, a)| (p.as_path(), *a))
            .collect::<Vec<_>>(),
        [
            (Path::new("/tmp/home/shared/data"), ReadWrite),
            (Path::new("/tmp/home/shared/data"), ReadOnly),
            (Path::new("/tmp/home/shared/data/child"), ReadWrite),
        ]
    );
    assert_eq!(b.access(&allowed.join("x")), Some(ReadOnly));
    assert_eq!(b.access(&child.join("x")), Some(ReadWrite));
}

#[test]
fn thread_ro_identity_is_rechecked_before_alias_projection() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().canonicalize().unwrap();
    let allowed = home.join("data");
    let child = allowed.join("locked");
    fs::create_dir_all(&child).unwrap();
    let b = boundary(&home)
        .with_allowed(LivePaths::new(vec![allowed]), PathAccess::ReadWrite)
        .with_thread_read_directory(ThreadReadDirectory::capture(&child).unwrap());
    assert!(
        aliases(&b)
            .iter()
            .any(|(_, _, a)| *a == PathAccess::ReadOnly)
    );
    fs::rename(&child, home.join("old")).unwrap();
    symlink(home.join("old"), &child).unwrap();
    assert!(
        !aliases(&b)
            .iter()
            .any(|(_, _, a)| *a == PathAccess::ReadOnly)
    );
}

#[test]
fn cache_alias_classification_covers_destination_and_canonical_source_relationships() {
    use PathAccess::{ReadOnly, ReadWrite};
    for different_destination in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let cache = home.join(".cargo/registry/src");
        let source = if different_destination {
            home.join("shared/src")
        } else {
            cache.clone()
        };
        fs::create_dir_all(source.join("deep/locked")).unwrap();
        fs::create_dir_all(source.join("deep/writable")).unwrap();
        fs::create_dir_all(source.join("deep/project")).unwrap();
        if different_destination {
            fs::create_dir_all(cache.parent().unwrap()).unwrap();
            symlink(&source, &cache).unwrap();
        }
        for relation in ["same", "child", "ancestor"] {
            let allowed = match relation {
                "same" => source.clone(),
                "child" => source.parent().unwrap().into(),
                _ => source.join("deep"),
            };
            for access in [ReadOnly, ReadWrite] {
                let b = boundary(&home).with_allowed(LivePaths::new(vec![allowed.clone()]), access);
                let plan = b.mount_plan(None, false);
                let cache_at = plan.mounts.iter().position(|m| {
                    m.cache && m.sandbox == Path::new("/tmp/home/.cargo/registry/src")
                });
                assert_eq!(
                    cache_at.is_some(),
                    different_destination || relation == "ancestor",
                    "different={different_destination}, relation={relation}, access={access:?}"
                );
                if let Some(at) = cache_at {
                    let allowed_at = plan
                        .mounts
                        .iter()
                        .position(|m| m.sandbox == allowed)
                        .unwrap();
                    assert!(at < allowed_at);
                }
                let mut constrained = b
                    .clone()
                    .read_only(source.join("deep/locked"))
                    .read_write(source.join("deep/writable"));
                constrained.workspace_root = source.join("deep/project");
                for b in [&constrained, &constrained.clone().read_only(&allowed)] {
                    let plan = b.mount_plan(None, false);
                    for relative in ["deep", "deep/locked", "deep/writable", "deep/project"] {
                        let host = source.join(relative);
                        let paths = [
                            Path::new("/tmp/home").join(host.strip_prefix(&home).unwrap()),
                            Path::new("/tmp/home/.cargo/registry/src").join(relative),
                        ];
                        for path in paths {
                            let effective = plan
                                .mounts
                                .iter()
                                .rev()
                                .find(|m| path.starts_with(&m.sandbox))
                                .map(|m| m.access);
                            assert_eq!(effective, b.access(&host), "{relation}, {path:?}");
                        }
                    }
                }
                let effective = |path: &Path| {
                    plan.mounts
                        .iter()
                        .rev()
                        .find(|m| path.starts_with(&m.sandbox))
                        .map(|m| m.access)
                };
                assert_eq!(
                    effective(Path::new("/tmp/home/.cargo/registry/src/deep")),
                    Some(access)
                );
                assert_eq!(b.access(&source.join("deep")), Some(access));
                if relation == "ancestor" {
                    assert_eq!(
                        effective(Path::new("/tmp/home/.cargo/registry/src/sibling")),
                        Some(ReadOnly)
                    );
                    assert_eq!(b.access(&source.join("sibling")), Some(ReadOnly));
                }
            }
        }
    }
}

#[test]
fn explicit_mirror_identity_survives_custom_cache_and_runtime_projections() {
    use PathAccess::{ReadOnly, ReadWrite};
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().canonicalize().unwrap();
    let custom = home.join("custom");
    let source = custom.join("registry/src");
    let a = home.join(".cargo/registry/src/sub");
    let b = source.join("sub/deep");
    fs::create_dir_all(a.join("deep")).unwrap();
    fs::create_dir_all(a.join("locked")).unwrap();
    fs::create_dir_all(&b).unwrap();
    for broad in [false, true] {
        for action in ["none", "ro", "rw", "workspace"] {
            fs::write(a.join("deep/x"), "A").unwrap();
            fs::write(b.join("x"), "B").unwrap();
            let mut boundary = boundary(&home)
                .cargo_cache(&custom)
                .with_allowed(
                    LivePaths::new(vec![
                        a.clone(),
                        if broad { source.clone() } else { b.clone() },
                    ]),
                    ReadWrite,
                )
                .read_only(a.join("locked"));
            boundary = match action {
                "ro" => boundary.read_only(&source),
                "rw" => boundary.read_only(&source).read_write(&source),
                "workspace" => {
                    boundary.workspace_root = source.clone();
                    boundary
                }
                _ => boundary,
            };
            let plan = boundary.mount_plan(None, false);
            let resolve = |path: &Path| {
                let mount = plan
                    .mounts
                    .iter()
                    .rev()
                    .find(|m| path.starts_with(&m.sandbox))
                    .unwrap();
                (
                    mount.host.join(path.strip_prefix(&mount.sandbox).unwrap()),
                    mount.access,
                )
            };
            let a_alias = Path::new("/tmp/home/.cargo/registry/src/sub/deep/x");
            let b_alias = Path::new("/tmp/home/custom/registry/src/sub/deep/x");
            assert_eq!(
                resolve(a_alias),
                (a.join("deep/x"), ReadWrite),
                "{broad} {action}"
            );
            assert_eq!(resolve(b_alias).0, b.join("x"));
            assert_eq!(Some(resolve(b_alias).1), boundary.access(&b.join("x")));
            assert_eq!(
                resolve(Path::new("/tmp/home/.cargo/registry/src/sub/locked/x")),
                (a.join("locked/x"), ReadOnly)
            );
            assert_eq!(fs::read_to_string(resolve(a_alias).0).unwrap(), "A");
            assert_eq!(fs::read_to_string(resolve(b_alias).0).unwrap(), "B");
            fs::write(resolve(a_alias).0, "updated-A").unwrap();
            assert_eq!(fs::read_to_string(a.join("deep/x")).unwrap(), "updated-A");
            assert_eq!(fs::read_to_string(b.join("x")).unwrap(), "B");
        }
    }
}
