use super::*;
use sandbox::ThreadReadDirectory;
use std::path::PathBuf;

fn success(output: Output) {
    assert!(output.status.success(), "{output:?}");
}

fn fixture_home(name: &str) -> Option<PathBuf> {
    const FIXTURE: &str = "EVORCH_BWRAP_HOME_FIXTURE";
    if std::env::var_os(FIXTURE).is_none() {
        // Only the child receives a fixture HOME. Parallel tests never change
        // the process environment, and no test mounts the user's real HOME.
        let home = tempdir().unwrap();
        success(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    &format!("home_mirrors::{name}"),
                    "--nocapture",
                ])
                .env(FIXTURE, home.path())
                .env("HOME", home.path())
                .env("CARGO_HOME", home.path().join(".cargo"))
                .env("RUSTUP_HOME", home.path().join(".rustup"))
                .output()
                .unwrap(),
        );
        return None;
    }
    Some(
        PathBuf::from(std::env::var_os(FIXTURE).unwrap())
            .canonicalize()
            .unwrap(),
    )
}

#[test]
#[ignore = "requires working bwrap namespaces"]
fn home_mirrors_share_targets_and_enforce_overrides() {
    let Some(home) = fixture_home("home_mirrors_share_targets_and_enforce_overrides") else {
        return;
    };
    let targets = home.join(".cargo/targets");
    let locked = targets.join("locked");
    let deep = home.join(".cargo/registry/src/index/dep");
    let rustup = home.join(".rustup/toolchains/stable");
    for path in [&locked, &deep, &rustup] {
        fs::create_dir_all(path).unwrap();
    }
    fs::write(home.join(".cargo/credentials.toml"), "private credential").unwrap();
    fs::write(home.join(".cargo/config.toml"), "private config").unwrap();
    let workspace = workspace();
    let boundary = || SandboxBoundary::new(workspace.path().into());
    let paths = LivePaths::new(vec![targets.clone()]);
    let scratch1 = tempdir().unwrap();
    let scratch2 = tempdir().unwrap();
    let make = |scratch: &Path, access, caches: bool| {
        let mut b = boundary()
            .with_allowed(paths.clone(), access)
            .with_thread_read_directory(ThreadReadDirectory::capture(&locked).unwrap())
            .with_scratch(scratch);
        if !caches {
            b = b.without_tool_caches();
        }
        BwrapSandbox::detect(BwrapConfig::from_boundary(b)).unwrap()
    };
    let first = make(scratch1.path(), PathAccess::ReadWrite, true);
    let second = make(scratch2.path(), PathAccess::ReadWrite, false);
    success(sh(
        &first,
        r#"set -eu
        test "$HOME" = /tmp/home
        test "$CARGO_HOME" = /tmp/home/.cargo
        test "$RUSTUP_HOME" = /tmp/home/.rustup
        test ! -e "$CARGO_HOME/credentials.toml"
        test ! -e "$CARGO_HOME/config.toml"
        echo shared > "$CARGO_HOME/targets/sentinel"
    "#,
    ));
    assert_eq!(
        fs::read_to_string(targets.join("sentinel")).unwrap(),
        "shared\n"
    );
    success(sh(
        &second,
        r#"test "$(cat "$CARGO_HOME/targets/sentinel")" = shared"#,
    ));
    assert!(
        !scratch1
            .path()
            .join("home/.cargo/targets/sentinel")
            .exists()
    );
    assert!(
        !scratch2
            .path()
            .join("home/.cargo/targets/sentinel")
            .exists()
    );
    assert!(
        !sh(&first, r#"echo forbidden > "$CARGO_HOME/targets/locked/x""#)
            .status
            .success()
    );
    let readonly = make(scratch2.path(), PathAccess::ReadOnly, false);
    assert!(
        !sh(&readonly, r#"echo forbidden > "$CARGO_HOME/targets/x""#)
            .status
            .success()
    );
    let project = targets.join("project");
    fs::create_dir(&project).unwrap();
    let workspace_override = BwrapSandbox::detect(BwrapConfig::from_boundary(
        SandboxBoundary::new(project)
            .without_tool_caches()
            .with_allowed(paths.clone(), PathAccess::ReadOnly),
    ))
    .unwrap();
    success(sh(
        &workspace_override,
        r#"echo project > "$CARGO_HOME/targets/project/sentinel""#,
    ));
    assert!(
        !sh(
            &workspace_override,
            r#"echo forbidden > "$CARGO_HOME/targets/outside-project""#
        )
        .status
        .success()
    );
    let rw_override = BwrapSandbox::detect(BwrapConfig::from_boundary(
        boundary()
            .without_tool_caches()
            .with_allowed(paths.clone(), PathAccess::ReadOnly)
            .read_write(&locked),
    ))
    .unwrap();
    success(sh(
        &rw_override,
        r#"echo writable > "$CARGO_HOME/targets/locked/sentinel""#,
    ));
    let network = first.with_network_access().unwrap();
    success(run(network
        .wrap(CommandSpec {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                "test -f \"$CARGO_HOME/targets/sentinel\"".into(),
            ],
            cwd: None,
            extra_env: vec![],
        })
        .unwrap()));

    paths.set(vec![deep.clone()]);
    success(sh(
        &first,
        r#"echo deep > "$CARGO_HOME/registry/src/index/dep/sentinel""#,
    ));
    assert_eq!(fs::read_to_string(deep.join("sentinel")).unwrap(), "deep\n");
    assert!(
        !sh(
            &first,
            r#"echo forbidden > "$CARGO_HOME/registry/src/sibling""#
        )
        .status
        .success()
    );
    success(sh(&first, r#"test ! -f "$CARGO_HOME/targets/sentinel""#));
    paths.set(vec![home.join(".cargo")]);
    success(sh(
        &first,
        r#"set -eu
        test "$(cat "$CARGO_HOME/config.toml")" = "private config"
        echo explicit > "$CARGO_HOME/registry/src/sibling"
    "#,
    ));
    paths.set(vec![]);
    success(sh(
        &first,
        r#"test ! -e "$CARGO_HOME/credentials.toml" && test ! -e "$CARGO_HOME/config.toml""#,
    ));

    // A deep Rustup mirror must override both RO aliases when PID-isolated.
    paths.set(vec![rustup.clone()]);
    let isolated = BwrapSandbox::detect(
        BwrapConfig::from_boundary(boundary().with_allowed(paths.clone(), PathAccess::ReadWrite))
            .isolate_processes(),
    )
    .unwrap();
    success(sh(
        &isolated,
        r#"echo rustup > "$RUSTUP_HOME/toolchains/stable/sentinel""#,
    ));
    assert_eq!(
        fs::read_to_string(rustup.join("sentinel")).unwrap(),
        "rustup\n"
    );

    // A symlinked cache keeps Cargo's different alias, with the same access
    // and thread restriction as the canonical HOME mirror and original path.
    let linked = home.join("shared/index");
    let linked_locked = linked.join("deep/locked");
    fs::create_dir_all(&linked_locked).unwrap();
    std::os::unix::fs::symlink(&linked, home.join(".cargo/registry/index")).unwrap();
    let linked_sandbox = BwrapSandbox::detect(BwrapConfig::from_boundary(
        boundary()
            .with_allowed(paths.clone(), PathAccess::ReadWrite)
            .with_thread_read_directory(ThreadReadDirectory::capture(&linked_locked).unwrap()),
    ))
    .unwrap();
    for allowed in [linked.clone(), home.join("shared"), linked.join("deep")] {
        paths.set(vec![allowed.clone()]);
        success(sh(
            &linked_sandbox,
            r#"set -eu
            echo canonical > "$HOME/shared/index/deep/sentinel"
            test "$(cat "$CARGO_HOME/registry/index/deep/sentinel")" = canonical
            echo cache > "$CARGO_HOME/registry/index/deep/sentinel"
            test "$(cat "$HOME/shared/index/deep/sentinel")" = cache
        "#,
        ));
        for alias in ["$HOME/shared/index", "$CARGO_HOME/registry/index"] {
            assert!(
                !sh(
                    &linked_sandbox,
                    &format!("echo forbidden > \"{alias}/deep/locked/x\"")
                )
                .status
                .success()
            );
        }
        if allowed == linked.join("deep") {
            assert!(
                !sh(
                    &linked_sandbox,
                    r#"echo forbidden > "$CARGO_HOME/registry/index/sibling""#
                )
                .status
                .success()
            );
        }
    }

    paths.set(vec![home.join(".cache/tool/data")]);
    fs::create_dir_all(home.join(".cache/tool/data")).unwrap();
    success(sh(
        &second,
        r#"echo deep > "$HOME/.cache/tool/data/sentinel""#,
    ));
    assert_eq!(
        fs::read_to_string(home.join(".cache/tool/data/sentinel")).unwrap(),
        "deep\n"
    );
    // A sandbox-created symlink or file must not redirect parent preparation.
    for collision in ["symlink", "file"] {
        let scratch = tempdir().unwrap();
        let sandbox = make(scratch.path(), PathAccess::ReadWrite, false);
        let conflict = scratch.path().join("home/.cache");
        if collision == "symlink" {
            std::os::unix::fs::symlink(&home, conflict).unwrap();
        } else {
            fs::write(conflict, "").unwrap();
        }
        assert!(
            sandbox
                .wrap(CommandSpec {
                    program: "true".into(),
                    args: vec![],
                    cwd: None,
                    extra_env: vec![],
                })
                .is_err()
        );
        assert!(!home.join("tool").exists());
    }
}

#[test]
#[ignore = "requires bwrap and EVORCH_TEST_CARGO_FORK / EVORCH_TEST_RUSTC store binaries"]
fn cargo_fork_reuses_one_pool_from_two_private_homes() {
    let Some(home) = fixture_home("cargo_fork_reuses_one_pool_from_two_private_homes") else {
        return;
    };
    let cargo = std::env::var("EVORCH_TEST_CARGO_FORK").expect("fork cargo store path");
    let rustc = std::env::var("EVORCH_TEST_RUSTC").expect("rustc store path");
    let workspace = workspace();
    fs::create_dir(workspace.path().join("src")).unwrap();
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = 'home_pool_probe'\nversion = '0.1.0'\nedition = '2024'\n[workspace]\n",
    )
    .unwrap();
    fs::write(
        workspace.path().join("src/lib.rs"),
        "pub fn probe() -> u8 { 42 }\n",
    )
    .unwrap();
    let targets = home.join(".cargo/targets");
    fs::create_dir_all(&targets).unwrap();
    let scratch1 = tempdir().unwrap();
    let scratch2 = tempdir().unwrap();
    let build = |scratch: &Path| {
        let boundary = SandboxBoundary::new(workspace.path().into())
            .without_tool_caches()
            .with_allowed(LivePaths::new(vec![targets.clone()]), PathAccess::ReadWrite)
            .with_scratch(scratch);
        let sandbox = BwrapSandbox::detect(BwrapConfig::from_boundary(boundary)).unwrap();
        success(run(sandbox
            .wrap(CommandSpec {
                program: cargo.clone(),
                args: vec!["build".into(), "--offline".into()],
                cwd: None,
                extra_env: vec![
                    ("RUSTC".into(), rustc.clone()),
                    ("RUSTC_WRAPPER".into(), "".into()),
                    ("CARGO_FORK_AUTO_UPDATE".into(), "0".into()),
                ],
            })
            .unwrap()));
        assert_eq!(
            fs::read_dir(scratch.join("home/.cargo/targets"))
                .unwrap()
                .count(),
            0
        );
    };
    build(scratch1.path());
    let before = pool_artifacts(&targets);
    assert!(
        !before.is_empty(),
        "fork must write intermediate artifacts in shared targets"
    );
    build(scratch2.path());
    assert_eq!(pool_artifacts(&targets), before);
}

fn pool_artifacts(root: &Path) -> Vec<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            } else if entry
                .file_name()
                .to_string_lossy()
                .contains("home_pool_probe")
                && entry
                    .path()
                    .extension()
                    .is_some_and(|e| e == "rlib" || e == "rmeta")
            {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    files
}
