//! Durable transaction fixture, not product metadata.
use std::collections::BTreeSet;
use storage::recovery::restart::RestartMetadata;
pub fn metadata() -> RestartMetadata {
    RestartMetadata {
        restart_id: "restart".into(),
        capture_id: "capture".into(),
        config_identity: "config".into(),
        old_epoch: 1,
        successor_epoch: 2,
        old_build: "old".into(),
        successor_build: "new".into(),
        target_identity: "target".into(),
        handoff_version: core_instance::handoff::HANDOFF_FORMAT_VERSION,
        snapshot_version: 1,
        storage_version: 62,
        old_builtin_tool_names: BTreeSet::from(["read".into()]),
        old_compiled_catalog_fingerprint: None,
        new_compiled_catalog_fingerprint: None,
        old_effective_tools_fingerprint: None,
        new_effective_tools_fingerprint: None,
    }
}
