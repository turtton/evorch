//! Host service of the `browser` area (PR 16a-2 / 16b). Placeholder: the namespace is reserved,
//! so nothing reaches this service and it starts no task, opens no file and
//! talks to nothing. The PR that implements the area fills in this file, the
//! adapter `ingress/namespaces/browser.rs` and the protocol module, and nothing
//! shared.
//!
//! Reserved for the renderer and browser work; takes nothing yet.
use super::AreaHandles;
use crate::bootstrap::BootstrapError;

/// The `browser` service, reached from `ingress/namespaces/browser.rs` as
/// `self.areas.browser`.
#[derive(Default)]
pub struct BrowserArea {}

impl BrowserArea {
    pub(super) fn new(_handles: &AreaHandles) -> Result<Self, BootstrapError> {
        Ok(Self::default())
    }
}
