#![cfg(target_os = "linux")]
#[path = "../shared-test-support/exec_fixture.rs"]
mod exec_fixture;
use core_launcher::handoff::HandoffDescription;
use std::process::Command;
#[test]
fn product_description_is_hidden_canonical_and_has_no_bootstrap_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let executable_dir = tempfile::tempdir().unwrap();
    let product = exec_fixture::product_copy(executable_dir.path());
    let output = Command::new(&product)
        .args(["--describe-handoff", "--config-dir"])
        .arg(dir.path().join("absent-config"))
        .env("XDG_STATE_HOME", dir.path().join("absent-state"))
        .env("XDG_RUNTIME_DIR", dir.path().join("absent-runtime"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let description: HandoffDescription = serde_json::from_slice(&output.stdout).unwrap();
    let range = storage::supported_schema_versions().unwrap();
    assert_eq!(description.schema.minimum, *range.start());
    assert_eq!(description.schema.maximum, *range.end());
    assert_eq!(
        description.protocol,
        workbench_protocol::ProtocolVersion::CURRENT.0
    );
    assert_eq!(
        description.build_identity,
        core_instance::handoff::elf_identity(&product).unwrap()
    );
    let fingerprint =
        storage::recovery::restart::restart_digest(&runtime::compiled_standard_tool_catalog())
            .unwrap();
    assert_eq!(
        description.tool_catalog,
        format!("standard-tools-v1:{fingerprint}")
    );
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "no DB, lock, owner, socket, receipt or config write"
    );
    let help = Command::new(&product).arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(!String::from_utf8_lossy(&help.stdout).contains("--describe-handoff"));
}
