//! Host service of the `memory` area (PR 13 reads, PR 14 updates). Placeholder: the namespace is reserved,
//! so nothing reaches this service and it starts no task, opens no file and
//! talks to nothing. The PR that implements the area fills in this file, the
//! adapter `ingress/namespaces/memory.rs` and the protocol module, and nothing
//! shared.
//!
//! Takes from the handles: `runtime` (its storage reader) for the read side and `storage` (the writer handle; no writer of its own) for the update side.
use super::AreaHandles;
use crate::bootstrap::BootstrapError;

/// The `memory` service, reached from `ingress/namespaces/memory.rs` as
/// `self.areas.memory`.
#[derive(Default)]
pub struct MemoryArea {}

impl MemoryArea {
    pub(super) fn new(_handles: &AreaHandles) -> Result<Self, BootstrapError> {
        Ok(Self::default())
    }
}
