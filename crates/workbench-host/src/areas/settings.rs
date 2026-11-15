//! Host service of the `settings` area (PR 10). Placeholder: the namespace is reserved,
//! so nothing reaches this service and it starts no task, opens no file and
//! talks to nothing. The PR that implements the area fills in this file, the
//! adapter `ingress/namespaces/settings.rs` and the protocol module, and nothing
//! shared.
//!
//! Takes from the handles: `config`, `config_dir`, `project_root`, `reload_models` (the one recomposition the Evaluator's reload tool uses too) and `credentials`.
use super::AreaHandles;
use crate::bootstrap::BootstrapError;

/// The `settings` service, reached from `ingress/namespaces/settings.rs` as
/// `self.areas.settings`.
#[derive(Default)]
pub struct SettingsArea {}

impl SettingsArea {
    pub(super) fn new(_handles: &AreaHandles) -> Result<Self, BootstrapError> {
        Ok(Self::default())
    }
}
