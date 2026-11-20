#![cfg(target_os = "linux")]
fn shell() -> std::ffi::OsString {
    std::env::var_os("EVORCH_TEST_SHELL").unwrap_or_else(|| {
        // The background-list fixtures were written for bash; under dash (the CI /bin/sh) the wnowait fixture fails.
        let bash = std::path::Path::new("/bin/bash");
        if bash.exists() {
            bash
        } else {
            std::path::Path::new("/bin/sh")
        }
        .into()
    })
}
use process_guard::{
    COMPAT_ID, GuardRuntime, LaunchSpec,
    image::{GuardImage, digest},
};
use std::{
    fs,
    io::Read,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
fn source() -> PathBuf {
    std::env::var_os(process_guard::BUNDLE_ENV)
        .map(PathBuf::from)
        .expect("prepare a guard bundle before running delivery tests")
}
fn copy_bundle(dir: &Path) -> PathBuf {
    let src = source();
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&src).unwrap()).unwrap();
    let helper = src
        .parent()
        .unwrap()
        .join(json["helper"]["file"].as_str().unwrap());
    let dest = dir.join("evorch-process-guard");
    if dest.exists() {
        fs::remove_file(&dest).unwrap();
    }
    fs::copy(helper, dest).unwrap();
    fs::set_permissions(
        dir.join("evorch-process-guard"),
        fs::Permissions::from_mode(0o555),
    )
    .unwrap();
    json["purpose"] = "test-helper".into();
    let release = json["helper"]["profile"] == "release";
    let mut recipe = vec!["build", "--locked"];
    if release {
        recipe.push("--release");
    }
    recipe.extend(["-p", "process-guard", "--bin", "evorch-process-guard"]);
    json["helper"]["build_args"] = serde_json::json!(recipe);
    json["helper"]["file"] = "evorch-process-guard".into();
    json["callers"] = serde_json::json!([]);
    let path = dir.join("guard-bundle.json");
    if path.exists() {
        fs::remove_file(&path).unwrap();
    }
    fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    path
}
fn replace_json(path: &Path, mutate: impl FnOnce(&mut serde_json::Value)) {
    let mut json = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    mutate(&mut json);
    let tmp = path.with_extension("new");
    fs::write(&tmp, serde_json::to_vec(&json).unwrap()).unwrap();
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o444)).unwrap();
    fs::rename(tmp, path).unwrap();
}
#[test]
fn pin_and_pty_fd_contract() {
    let temp = tempfile::tempdir().unwrap();
    let path = copy_bundle(temp.path());
    let image = GuardImage::open(&path, None).unwrap();
    let replacement = temp.path().join("replacement");
    fs::write(&replacement, b"not the pinned ELF").unwrap();
    fs::set_permissions(&replacement, fs::Permissions::from_mode(0o555)).unwrap();
    fs::rename(replacement, temp.path().join("evorch-process-guard")).unwrap();
    assert!(
        GuardImage::open(&path, None).is_err(),
        "a new bind must reject the replacement"
    );
    let [dev, ino] = image.identity_args();
    let output = Command::new(image.inherited_exec_path())
        .args(["--describe-image", &dev, &ino])
        .env_clear()
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["guard_compat_id"],
        COMPAT_ID
    );
    let wrong = Command::new(image.inherited_exec_path())
        .args(["--describe-image", "0", "0"])
        .env_clear()
        .output()
        .unwrap();
    assert_eq!(wrong.status.code(), Some(125));
    assert!(wrong.stdout.is_empty() && wrong.stderr.is_empty());
    // portable-pty closes >2 BEFORE exec. Only the parent-owned locator works.
    for (locator, ok) in [
        (image.inherited_exec_path(), false),
        (image.pty_exec_path(), true),
    ] {
        let pty = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = portable_pty::CommandBuilder::new(locator);
        cmd.args(["--describe-image", &dev, &ino]);
        cmd.env_clear();
        cmd.cwd("/");
        let mut child = pty.slave.spawn_command(cmd).unwrap();
        let pid = child.process_id().unwrap();
        drop(pty.slave);
        let mut output = Vec::new();
        let mut reader = pty.master.try_clone_reader().unwrap();
        let status = child.wait().unwrap();
        let _ = reader.read_to_end(&mut output);
        assert_eq!(status.success(), ok);
        if ok {
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&output).unwrap()["guard_compat_id"],
                COMPAT_ID
            );
        }
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
    }
}
#[test]
fn malformed_or_mutable_bundle_is_not_a_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let path = copy_bundle(temp.path());
    for (field, value) in [
        ("schema_version", serde_json::json!(2)),
        ("guard_abi", serde_json::json!(99)),
        ("guard_compat_id", serde_json::json!("0".repeat(64))),
        ("target", serde_json::json!("wrong")),
    ] {
        let path = copy_bundle(temp.path());
        replace_json(&path, |j| j[field] = value);
        assert!(GuardImage::open(&path, None).is_err());
    }
    copy_bundle(temp.path());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(GuardImage::open(&path, None).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    let helper = temp.path().join("evorch-process-guard");
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(GuardImage::open(&path, None).is_err());
    fs::remove_file(&helper).unwrap();
    std::os::unix::fs::symlink("/bin/sh", &helper).unwrap();
    assert!(GuardImage::open(&path, None).is_err());
    fs::remove_file(helper).unwrap();
    assert!(GuardImage::open(&path, None).is_err());
    assert!(GuardImage::open(Path::new("guard-bundle.json"), None).is_err());
}
#[test]
fn installed_fixture() {
    if std::env::var_os("GUARD_INSTALLED_FIXTURE").is_none() {
        return;
    }
    let role = std::env::var("GUARD_INSTALLED_FIXTURE").unwrap();
    let runtime = GuardRuntime::bind_installed(&role).unwrap();
    let library = GuardRuntime::for_library(None).unwrap();
    assert_eq!(runtime.image().identity(), library.image().identity());
    let mut spec = LaunchSpec::new(shell()).unwrap();
    spec.argv.extend([b"-c".to_vec(), b":".to_vec()]);
    assert!(
        library
            .spawn(&spec, [Stdio::null(), Stdio::null(), Stdio::null()])
            .unwrap()
            .wait()
            .unwrap()
            .success()
    );
}
#[test]
fn three_actual_elf_layouts_ignore_environment_override() {
    for (role, relative) in [
        ("cli", "bin/.evorch-wrapped"),
        ("core", "bin/.evorch-core-wrapped"),
        ("gui", "libexec/evorch-gui-cef/evorch-gui"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let exe = temp.path().join(relative);
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o555)).unwrap();
        let path = copy_bundle(exe.parent().unwrap());
        let (size, sha) = digest(&mut fs::File::open(&exe).unwrap()).unwrap();
        replace_json(&path, |j| {
            j["purpose"] = "distribution".into();
            j["helper"]["profile"] = "release".into();
            j["callers"] = serde_json::json!([{"role":role,"file":exe.file_name().unwrap().to_str().unwrap(),"size_bytes":size,"sha256":sha}]);
            let mut args = vec!["build", "--locked", "--release"];
            let (packages, bins) = match role {
                "cli" => (
                    vec!["evorch", "evorch-core", "process-guard"],
                    vec!["evorch", "evorch-core", "evorch-process-guard"],
                ),
                "core" => (
                    vec!["evorch-core", "process-guard"],
                    vec!["evorch-core", "evorch-process-guard"],
                ),
                _ => (
                    vec!["gui", "browser-cef", "evorch-renderer", "process-guard"],
                    vec![
                        "evorch-gui",
                        "evorch-cef-helper",
                        "evorch-renderer",
                        "evorch-process-guard",
                    ],
                ),
            };
            for package in packages {
                args.extend(["-p", package]);
            }
            if role == "gui" {
                args.extend(["--features", "gui/cef,browser-cef/cef"]);
            }
            for bin in bins {
                args.extend(["--bin", bin]);
            }
            j["helper"]["build_args"] = serde_json::json!(args);
            if role == "cli" {
                let sibling = exe.parent().unwrap().join(".evorch-core-wrapped");
                fs::copy(&exe, &sibling).unwrap();
                j["callers"].as_array_mut().unwrap().push(serde_json::json!({"role":"core","file":".evorch-core-wrapped","size_bytes":size,"sha256":sha}));
            }
        });
        let status = Command::new(&exe)
            .args(["--exact", "installed_fixture", "--nocapture"])
            .env("GUARD_INSTALLED_FIXTURE", role)
            .env(process_guard::BUNDLE_ENV, "/no/env/override.json")
            .status()
            .unwrap();
        assert!(status.success(), "{role}");
        replace_json(&path, |j| j["callers"][0]["sha256"] = "0".repeat(64).into());
        let output = Command::new(&exe)
            .args(["--exact", "installed_fixture"])
            .env("GUARD_INSTALLED_FIXTURE", role)
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "mismatched running caller must fail"
        );
    }
}
