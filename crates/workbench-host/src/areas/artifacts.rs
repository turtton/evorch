//! Host service of the `artifacts` area (PR 16a-2). Placeholder: the namespace is reserved,
//! so nothing reaches this service and it starts no task, opens no file and
//! talks to nothing. The PR that implements the area fills in this file, the
//! adapter `ingress/namespaces/artifacts.rs` and the protocol module, and nothing
//! shared.
//!
//! Reserved for the renderer and artifact work; takes nothing yet.
use super::AreaHandles;
use crate::bootstrap::BootstrapError;

/// The `artifacts` service, reached from `ingress/namespaces/artifacts.rs` as
/// `self.areas.artifacts`.
#[derive(Default)]
pub struct ArtifactsArea {}

impl ArtifactsArea {
    pub(super) fn new(_handles: &AreaHandles) -> Result<Self, BootstrapError> {
        Ok(Self::default())
    }

    /// The provider behind `inspect_artifact`; see [`super::Areas::artifact_pages`].
    ///
    /// `None` until the area implements it. The implementation returns `Some`
    /// unconditionally (not depending on helper state), so the tool catalog
    /// never changes with the helper's life cycle.
    pub(super) fn artifact_pages(
        &self,
    ) -> Option<std::sync::Arc<dyn runtime::artifacts::ArtifactPages>> {
        None
    }
}
