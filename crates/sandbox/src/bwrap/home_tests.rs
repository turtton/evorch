use super::*;
use crate::boundary::{LivePaths, PathAccess};
use std::fs;
use std::os::unix::fs::symlink;

fn fixture(home: &Path, paths: LivePaths, access: PathAccess) -> BwrapSandbox {
    let mut boundary = SandboxBoundary::new(home.parent().unwrap().join("project"))
        .without_tool_caches()
        .with_allowed(paths, access);
    boundary.host_home = Some(home.into());
    super::tests::sandbox(BwrapConfig::from_boundary(boundary))
}

#[test]
fn wrap_mirrors_live_declarations_and_keeps_forced_environment_and_network_variant() {
    let home = tempfile::tempdir().unwrap();
    let first = home.path().join(".cargo/targets");
    let second = home.path().join(".cache/tool/data");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    for access in [PathAccess::ReadOnly, PathAccess::ReadWrite] {
        let live = LivePaths::new(vec![first.clone()]);
        let sandbox = fixture(home.path(), live.clone(), access);
        let spec = super::tests::spec(None);
        let before = sandbox.wrap(spec.clone()).unwrap();
        let flag = if access == PathAccess::ReadWrite {
            "--bind-try"
        } else {
            "--ro-bind-try"
        };
        let contains = |args: &[String], source: &Path, destination: &Path| {
            args.windows(3).any(|a| {
                a[0] == flag
                    && a[1] == source.to_string_lossy()
                    && a[2] == destination.to_string_lossy()
            })
        };
        assert!(contains(&before.args, &first, &first));
        assert!(contains(
            &before.args,
            &first,
            Path::new("/tmp/home/.cargo/targets")
        ));
        live.set(vec![second.clone()]);
        let after = sandbox.wrap(spec.clone()).unwrap();
        assert!(!after.args.iter().any(|a| a == &first.to_string_lossy()));
        assert!(contains(&after.args, &second, &second));
        assert!(contains(
            &after.args,
            &second,
            Path::new("/tmp/home/.cache/tool/data")
        ));
        assert!(sandbox.scratch.join("home/.cache/tool/data").is_dir());
        for (key, value) in [
            ("HOME", "/tmp/home"),
            ("CARGO_HOME", "/tmp/home/.cargo"),
            ("RUSTUP_HOME", "/tmp/home/.rustup"),
        ] {
            assert!(after.env.iter().any(|(k, v)| k == key && v == value));
        }
        let network = sandbox.with_network_access().unwrap().wrap(spec).unwrap();
        assert_eq!(
            network.args,
            after
                .args
                .into_iter()
                .filter(|a| a != "--unshare-net")
                .collect::<Vec<_>>()
        );
        assert_eq!(network.env, after.env);
    }
}

#[test]
fn preparation_rejects_symlinks_and_files_without_touching_host_parents() {
    for collision in ["link", "file"] {
        let home = tempfile::tempdir().unwrap();
        let allowed = home.path().join(".cache/tool/data");
        fs::create_dir_all(&allowed).unwrap();
        let sandbox = fixture(
            home.path(),
            LivePaths::new(vec![allowed]),
            PathAccess::ReadWrite,
        );
        let outside = tempfile::tempdir().unwrap();
        let conflict = sandbox.scratch.join("home/.cache");
        if collision == "link" {
            symlink(outside.path(), &conflict).unwrap();
        } else {
            fs::write(&conflict, "").unwrap();
        }
        let error = sandbox.wrap(super::tests::spec(None)).unwrap_err();
        assert!(matches!(error, SandboxError::InvalidSpec { .. }), "{error}");
        assert!(error.to_string().contains("/tmp/home/.cache"), "{error}");
        assert!(!outside.path().join("tool").exists());
    }
    // ensure_layout runs before the mirror plan; it too must not follow HOME links.
    let scratch = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), scratch.path().join("home")).unwrap();
    assert!(crate::scratch::ensure_layout(scratch.path()).is_err());
    assert!(!outside.path().join(".cargo").exists());
}

#[test]
fn preparation_fails_closed_when_an_original_bind_covers_private_home() {
    let home = tempfile::tempdir().unwrap();
    let allowed = home.path().join("data");
    fs::create_dir(&allowed).unwrap();
    for conflict in ["/", "/tmp", "/tmp/home", "/tmp/home/.cargo"] {
        let sandbox = fixture(
            home.path(),
            LivePaths::new(vec![allowed.clone(), PathBuf::from(conflict)]),
            PathAccess::ReadWrite,
        );
        assert!(
            matches!(
                sandbox.wrap(super::tests::spec(None)),
                Err(SandboxError::InvalidSpec { .. })
            ),
            "{conflict}"
        );
    }
}

#[test]
fn deep_child_of_ro_cache_prepares_only_private_parents_and_preserves_priority() {
    let home = tempfile::tempdir().unwrap();
    let deep = home.path().join(".cargo/registry/src/index/dep");
    fs::create_dir_all(&deep).unwrap();
    let mut sandbox = fixture(
        home.path(),
        LivePaths::new(vec![deep.clone()]),
        PathAccess::ReadWrite,
    );
    sandbox.config.boundary.cargo_home = Some(home.path().join(".cargo"));
    let command = sandbox.wrap(super::tests::spec(None)).unwrap();
    let mounts: Vec<_> = command
        .args
        .windows(3)
        .filter(|a| super::tests::is_mount(&a[0]))
        .collect();
    let parent = mounts
        .iter()
        .position(|a| a[0] == "--ro-bind" && a[2] == "/tmp/home/.cargo/registry/src")
        .unwrap();
    let child = mounts
        .iter()
        .position(|a| a[0] == "--bind-try" && a[2] == "/tmp/home/.cargo/registry/src/index/dep")
        .unwrap();
    assert!(parent < child);
    assert!(sandbox.scratch.join("home/.cargo/registry").is_dir());
    assert!(
        !sandbox
            .scratch
            .join("home/.cargo/registry/src/index")
            .exists()
    );
}

#[test]
fn vanished_allowed_sources_keep_try_binds_and_are_not_recreated() {
    let home = tempfile::tempdir().unwrap();
    let missing = home.path().join(".cache/tool/data");
    let sandbox = fixture(
        home.path(),
        LivePaths::new(vec![missing.clone()]),
        PathAccess::ReadWrite,
    );
    let command = sandbox.wrap(super::tests::spec(None)).unwrap();
    assert!(command.args.windows(3).any(|a| a[0] == "--bind-try"
        && a[1] == missing.to_string_lossy()
        && a[2] == "/tmp/home/.cache/tool/data"));
    assert!(!home.path().join(".cache").exists());
    assert!(!sandbox.scratch.join("home/.cache").exists());
}
