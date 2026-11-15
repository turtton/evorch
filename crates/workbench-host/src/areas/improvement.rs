//! Host service of the `improvement` area (PR 13 reads, PR 14 updates). Placeholder: the namespace is reserved,
//! so nothing reaches this service and it starts no task, opens no file and
//! talks to nothing. The PR that implements the area fills in this file, the
//! adapter `ingress/namespaces/improvement.rs` and the protocol module, and nothing
//! shared.
//!
//! Takes from the handles: the same as `memory`.
use super::AreaHandles;
use crate::bootstrap::BootstrapError;

/// The `improvement` service, reached from `ingress/namespaces/improvement.rs` as
/// `self.areas.improvement`.
#[derive(Default)]
pub struct ImprovementArea {}

impl ImprovementArea {
    pub(super) fn new(_handles: &AreaHandles) -> Result<Self, BootstrapError> {
        Ok(Self::default())
    }
}
