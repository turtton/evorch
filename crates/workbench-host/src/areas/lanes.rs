//! Host service of the `lanes` area. Empty until that area's PR adds the operations
//! this service backs: it holds no state, starts no task and publishes nothing. What the namespace offers is
//! decided by the operations its protocol module declares, never by this file.
//! See [`super`] for what the service may use.
use super::AreaHandles;
use crate::bootstrap::BootstrapError;

/// The `lanes` service, reached from `ingress/namespaces/lanes.rs` as
/// `self.areas.lanes`.
#[derive(Default)]
pub struct LanesArea {}

impl LanesArea {
    pub(super) fn new(_handles: &AreaHandles) -> Result<Self, BootstrapError> {
        Ok(Self::default())
    }
}
