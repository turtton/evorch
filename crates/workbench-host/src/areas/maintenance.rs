//! Host service of the `maintenance` area (PR 17d). Placeholder: the namespace is reserved,
//! so nothing reaches this service and it starts no task, opens no file and
//! talks to nothing. The PR that implements the area fills in this file, the
//! adapter `ingress/namespaces/maintenance.rs` and the protocol module, and nothing
//! shared.
//!
//! Takes from the handles: `storage` (the writer handle; no writer on a path of its own) and `runtime`.
use super::AreaHandles;
use crate::bootstrap::BootstrapError;

/// The `maintenance` service, reached from `ingress/namespaces/maintenance.rs` as
/// `self.areas.maintenance`.
#[derive(Default)]
pub struct MaintenanceArea {}

impl MaintenanceArea {
    pub(super) fn new(_handles: &AreaHandles) -> Result<Self, BootstrapError> {
        Ok(Self::default())
    }
}
