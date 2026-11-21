//! Real capture-checked ELF fixture shared by finalizer and lifecycle tests.
use core_launcher::handoff::{
    CurrentElf, HandoffDescription, ProbeLimits, SchemaRange,
    targets::{CaptureCheckedTarget, RestartTarget, TargetRequirements, TargetSelection},
};
use std::{os::unix::fs::PermissionsExt, path::Path, sync::Arc};
fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}
pub async fn checked_target(dir: &Path) -> (Arc<CurrentElf>, CaptureCheckedTarget) {
    // This child only describes a real ELF. The call count independently proves
    // that stage 6 does NOT reprobe/spawn after the final spawn fence.
    static CURRENT: std::sync::OnceLock<Arc<CurrentElf>> = std::sync::OnceLock::new();
    let current = CURRENT
        .get_or_init(|| Arc::new(CurrentElf::record().unwrap()))
        .clone();
    // Use a small real ELF rather than repeatedly hashing this large debug test
    // binary. Neither this describing wrapper nor that ELF is ever execed.
    let elf = dir.join("described-elf");
    std::fs::copy("/bin/sh", &elf).unwrap();
    std::fs::set_permissions(&elf, std::fs::Permissions::from_mode(0o700)).unwrap();
    let d = HandoffDescription {
        version: 1,
        build_identity: core_instance::handoff::elf_identity(&elf).unwrap(),
        elf_path: elf,
        handoff_format: 1,
        snapshot_format: 1,
        schema: SchemaRange {
            minimum: 1,
            maximum: 1000,
        },
        protocol: 1,
        platform: format!("linux-{}", std::env::consts::ARCH),
        tool_catalog: "fixture-catalog".into(),
    };
    let path = dir.join("target");
    let calls = dir.join("probes");
    let json = serde_json::to_string(&d).unwrap().replace('\'', "'\\''");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf 'probe\\n' >> {}\nprintf '%s\\n' '{}'\n",
            quote(&calls),
            json,
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let checked = RestartTarget::preflight(
        TargetSelection {
            id: "target",
            configured_path: Some(&path),
            timeout: std::time::Duration::from_secs(300),
        },
        &current,
        TargetRequirements {
            handoff: 1,
            snapshot: 1,
            storage_schema: 62,
            protocol: 1,
            platform: &d.platform,
        },
        ProbeLimits::default(),
        std::future::pending(),
    )
    .await
    .unwrap()
    .before_capture(&current, ProbeLimits::default(), std::future::pending())
    .await
    .unwrap();
    (current, checked)
}
