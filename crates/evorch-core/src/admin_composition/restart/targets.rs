//! Local allowlist composition; target validation remains exclusively in #710.
#[cfg(target_os = "linux")]
use crate::restart::public::{LocalTargets, Requirements};
#[cfg(target_os = "linux")]
use core_launcher::handoff::{CurrentElf, ProbeLimits};
#[cfg(target_os = "linux")]
use std::{path::PathBuf, sync::Arc};

/// The caller shares the recorded current ELF with finalization. No wire field
/// supplies a directory, target path, argv, environment or probe limits.
#[cfg(target_os = "linux")]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Only until V1b-2b connects this leaf; remove then"
    )
)]
pub fn build(
    directory: PathBuf,
    current: Arc<CurrentElf>,
) -> Result<Arc<LocalTargets>, workbench_host::bootstrap::BootstrapError> {
    let description = crate::handoff_description::describe()?;
    Ok(Arc::new(LocalTargets::new(
        directory,
        current,
        Requirements {
            handoff: description.handoff_format,
            snapshot: description.snapshot_format,
            storage_schema: description.schema.maximum,
            protocol: description.protocol,
            platform: description.platform,
        },
        ProbeLimits::default(),
    )))
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
