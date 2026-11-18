//! Inert inputs for the artifact API. F2 enables the provider in this leaf;
//! bootstrap already passes the runtime's store and the browser's single owner.
use super::browser::renderer::RendererOwner;
use std::sync::Arc;

#[derive(Default)]
pub struct ArtifactsArea {
    pub store: Option<Arc<runtime::artifacts::ArtifactStore>>,
    pub renderer: Option<RendererOwner>,
}
impl ArtifactsArea {
    pub(super) fn new(
        store: Arc<runtime::artifacts::ArtifactStore>,
        renderer: Option<RendererOwner>,
    ) -> Self {
        Self {
            store: Some(store),
            renderer,
        }
    }
    /// Remains disabled until F2. Then return Some unconditionally, regardless
    /// of helper presence/state, using the already shared inputs above.
    pub(super) fn artifact_pages(&self) -> Option<Arc<dyn runtime::artifacts::ArtifactPages>> {
        None
    }
}
