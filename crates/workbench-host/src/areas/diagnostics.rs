//! Host service of the `diagnostics` area (PR 13). Placeholder: the namespace is reserved,
//! so nothing reaches this service and it starts no task, opens no file and
//! talks to nothing. The PR that implements the area fills in this file, the
//! adapter `ingress/namespaces/diagnostics.rs` and the protocol module, and nothing
//! shared.
//!
//! Takes from the handles: `runtime` (its storage reader), `storage` and `database`; reads only.
use super::AreaHandles;
use crate::bootstrap::BootstrapError;

/// The `diagnostics` service, reached from `ingress/namespaces/diagnostics.rs` as
/// `self.areas.diagnostics`.
#[derive(Default)]
pub struct DiagnosticsArea {}

impl DiagnosticsArea {
    pub(super) fn new(_handles: &AreaHandles) -> Result<Self, BootstrapError> {
        Ok(Self::default())
    }
}
