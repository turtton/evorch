#![cfg(target_os = "linux")]
use std::{fs, process::Command};
#[test]
fn startup_bind_precedes_gui_and_ignores_target_environment() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("evorch-gui");
    fs::copy(std::env::var_os("CARGO_BIN_EXE_evorch-gui").unwrap(), &exe).unwrap();
    let run = || {
        Command::new(&exe)
            .arg("--guard-entry-invalid")
            .env("EVORCH_PROCESS_GUARD_BUNDLE", "/missing")
            .output()
            .unwrap()
    };
    let absent = run();
    assert_eq!(absent.status.code(), Some(1));
    assert_eq!(
        absent.stderr,
        b"evorch-gui: process guard bundle validation failed\n"
    );
    assert!(
        Command::new(&exe)
            .arg("--help")
            .output()
            .unwrap()
            .status
            .success()
    );
    let manifest = test_support::guard_bundle::bind_copy(&exe, "gui").unwrap();
    let valid = run();
    assert_eq!(valid.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&valid.stderr).contains("argument error: unknown option"));
    let mut bundle: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    bundle["callers"][0]["sha256"] = "0".repeat(64).into();
    let next = manifest.with_extension("new");
    fs::write(&next, serde_json::to_vec(&bundle).unwrap()).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&next, fs::Permissions::from_mode(0o444)).unwrap();
    fs::rename(next, manifest).unwrap();
    assert_eq!(run().stderr, absent.stderr);
}
