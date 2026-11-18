//! Installation gate for the core owner real-helper suite. F1 replaces this
//! placeholder with production owner lifecycle and process-tree assertions.
#![cfg(all(unix, feature = "renderer-test-helper"))]

#[test]
#[ignore = "requires the actual renderer binary; cef-e2e supplies it"]
fn real_helper_is_installed_for_the_owner_suite() {
    let exe = std::env::var_os("EVORCH_RENDERER_EXE")
        .map(std::path::PathBuf::from)
        .expect("cef-e2e must set EVORCH_RENDERER_EXE");
    assert!(
        exe.is_absolute() && exe.is_file(),
        "build evorch-renderer first"
    );
    let output = std::process::Command::new(exe)
        .args(["--cache-dir", "relative"])
        .output()
        .expect("execute the actual helper");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("absolute path"));
}
