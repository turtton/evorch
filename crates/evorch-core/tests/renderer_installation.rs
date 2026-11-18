//! The real core entry must finish diagnostics before ownership/bootstrap.
use std::{
    os::unix::{ffi::OsStringExt, fs::PermissionsExt},
    path::PathBuf,
    process::Command,
};

fn core() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_evorch-core")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("evorch-core")
        })
}

#[test]
fn real_entry_prints_override_without_executing_it_or_creating_core_state() {
    let temp = tempfile::tempdir().unwrap();
    let helper = temp.path().join("helper with spaces");
    let started = temp.path().join("helper-started");
    std::fs::write(
        &helper,
        format!("#!/bin/sh\nprintf started > '{}'\n", started.display()),
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = temp.path().join("must-not-be-created");
    let output = Command::new(core())
        .args(["--print-renderer-helper", "--config-dir"])
        .arg(&config)
        .args(["--launch-fd", "999"])
        .env("EVORCH_RENDERER_EXEC", &helper)
        .env("XDG_STATE_HOME", temp.path().join("no-state"))
        .env("HOME", temp.path().join("no-home"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    let mut expected = helper.as_os_str().as_encoded_bytes().to_vec();
    expected.push(b'\n');
    assert_eq!(output.stdout, expected);
    assert!(output.stderr.is_empty());
    assert!(!config.exists());
    assert!(!started.exists());
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
}

#[test]
fn real_entry_uses_colocated_default_and_preserves_non_utf8_override() {
    let exe = core();
    let output = Command::new(&exe)
        .arg("--print-renderer-helper")
        .env_remove("EVORCH_RENDERER_EXEC")
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    let expected = exe
        .canonicalize()
        .unwrap()
        .with_file_name("evorch-renderer");
    assert_eq!(
        output.stdout,
        format!("{}\n", expected.display()).as_bytes()
    );
    let path = std::ffi::OsString::from_vec(b"/absolute/non-utf8-\xff".to_vec());
    let output = Command::new(&exe)
        .arg("--print-renderer-helper")
        .env("EVORCH_RENDERER_EXEC", path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"/absolute/non-utf8-\xff\n");
}

#[test]
fn invalid_override_exits_nonzero_with_reason_and_no_bootstrap_side_effects() {
    let temp = tempfile::tempdir().unwrap();
    for invalid in ["", "renderer", "./renderer"] {
        let output = Command::new(core())
            .args(["--print-renderer-helper", "--config-dir"])
            .arg(temp.path().join("no-config"))
            .env("EVORCH_RENDERER_EXEC", invalid)
            .env("XDG_STATE_HOME", temp.path().join("no-state"))
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("EVORCH_RENDERER_EXEC must be a nonempty absolute executable path")
        );
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }
    let output = Command::new(core()).arg("--help").output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("--print-renderer-helper"));
    assert!(help.contains("read-only diagnostic"));
}
