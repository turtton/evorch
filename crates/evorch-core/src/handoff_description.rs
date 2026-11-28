//! Hidden read-only preflight; this is not a user capability or a run catalog.
use core_launcher::handoff::{HandoffDescription, SchemaRange};
use workbench_host::bootstrap::BootstrapError;

pub fn describe() -> Result<HandoffDescription, BootstrapError> {
    let schema = storage::supported_schema_versions()?;
    let identity =
        storage::recovery::restart::restart_digest(&runtime::compiled_standard_tool_catalog())?;
    Ok(HandoffDescription::for_running(
        core_instance::handoff::HANDOFF_FORMAT_VERSION,
        1,
        SchemaRange {
            minimum: *schema.start(),
            maximum: *schema.end(),
        },
        workbench_protocol::ProtocolVersion::CURRENT.0,
        format!("standard-tools-v1:{identity}"),
    )?)
}
