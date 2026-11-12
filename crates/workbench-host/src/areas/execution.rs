//! Host service of the `execution` area. Empty until that area's PR adds the operations
//! this service backs: it holds no state, starts no task and publishes nothing. What the namespace offers is
//! decided by the operations its protocol module declares, never by this file.
//! See [`super`] for what the service may use.
use super::AreaHandles;
use crate::bootstrap::BootstrapError;

/// The `execution` service, reached from `ingress/namespaces/execution.rs` as
/// `self.areas.execution`.
#[derive(Default)]
pub struct ExecutionArea {}

impl ExecutionArea {
    pub(super) fn new(_handles: &AreaHandles) -> Result<Self, BootstrapError> {
        Ok(Self::default())
    }
}
