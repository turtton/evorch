#![cfg(target_os = "linux")]
//! Actual entry diagnostics: no config/DB bootstrap, but a required bound bundle.
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};
fn seal(path: &Path, value: &serde_json::Value) {
    let temp = path.with_extension("new");
    fs::write(&temp, serde_json::to_vec(value).unwrap()).unwrap();
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o444)).unwrap();
    fs::rename(temp, path).unwrap();
}
#[test]
fn diagnostic_bind_failure_is_fixed_help_is_available_and_environment_cannot_override() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("core");
    fs::copy(std::env::var_os("CARGO_BIN_EXE_evorch-core").unwrap(), &exe).unwrap();
    let run = |flag: &str| {
        Command::new(&exe)
            .args([
                "--config-dir",
                dir.path().join("absent").to_str().unwrap(),
                flag,
            ])
            .env("EVORCH_PROCESS_GUARD_BUNDLE", "/untrusted/override")
            .output()
            .unwrap()
    };
    for flag in ["--describe-handoff", "--print-renderer-helper"] {
        let out = run(flag);
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(
            out.stderr,
            b"evorch-core: process guard bundle validation failed\n"
        );
        assert!(out.stdout.is_empty());
    }
    assert!(run("--help").status.success());
    let manifest = test_support::guard_bundle::bind_copy(&exe, "core").unwrap();
    let mut bundle: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    for flag in ["--describe-handoff", "--print-renderer-helper"] {
        let out = run(flag);
        assert!(out.status.success(), "{flag}: {:?}", out.stderr);
        assert!(!out.stdout.is_empty());
    }
    bundle["callers"][0]["sha256"] = "0".repeat(64).into();
    seal(&manifest, &bundle);
    for flag in ["--describe-handoff", "--print-renderer-helper"] {
        let out = run(flag);
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(
            out.stderr,
            b"evorch-core: process guard bundle validation failed\n"
        );
    }
    assert!(run("--help").status.success());
    assert!(!dir.path().join("absent").exists());
}
