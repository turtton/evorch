//! Artifact reads and the stable runtime provider share the core's store/owner.
mod content;
use super::browser::renderer::RendererOwner;
pub(crate) use content::{metadata, not_found, preview_html, storage_error};
use std::sync::Arc;

#[derive(Default)]
pub struct ArtifactsArea {
    pub store: Option<Arc<runtime::artifacts::ArtifactStore>>,
    pub renderer: Option<RendererOwner>,
    provider: Arc<super::browser::run_pages::RuntimePages>,
}
impl ArtifactsArea {
    pub(crate) fn verified_record(
        &self,
        runtime: &runtime::AgentRuntime,
        reference: &workbench_protocol::namespaces::artifacts::ArtifactRef,
    ) -> Result<runtime::artifacts::ArtifactRecord, workbench_protocol::errors::WorkbenchError>
    {
        reference.validate().map_err(|_| not_found())?;
        let indexed = runtime
            .with_storage_reader(|db| {
                db.artifact_ref_for_thread(reference.thread.as_str(), &reference.artifact_id)
            })
            .map_err(|_| storage_error())?
            .flatten()
            .ok_or_else(not_found)?;
        if indexed.sha256 != reference.digest {
            return Err(not_found());
        }
        let record = self
            .store
            .as_ref()
            .ok_or_else(storage_error)?
            .load(&reference.artifact_id)
            .map_err(|_| not_found())?;
        if record.artifact_id != reference.artifact_id
            || record.sha256 != reference.digest
            || record.thread_id.as_deref() != Some(reference.thread.as_str())
            || indexed
                .run_id
                .as_ref()
                .is_some_and(|run| run != &record.run_id)
            || !content::supported_type(&record)
        {
            return Err(not_found());
        }
        Ok(record)
    }

    pub(crate) fn read_content(
        &self,
        record: &runtime::artifacts::ArtifactRecord,
        offset: u64,
        max_bytes: u32,
    ) -> Result<(String, bool), workbench_protocol::errors::WorkbenchError> {
        content::read_text(
            self.store.as_ref().ok_or_else(storage_error)?,
            record,
            offset,
            max_bytes,
        )
    }

    /// The blob of an image artifact for a data connection.
    pub(crate) fn image_blob(
        &self,
        record: &runtime::artifacts::ArtifactRecord,
    ) -> Result<
        (
            workbench_protocol::namespaces::browser::data::BlobStart,
            Box<dyn std::io::Read + Send>,
        ),
        workbench_protocol::errors::WorkbenchError,
    > {
        content::image_blob(self.store.as_ref().ok_or_else(storage_error)?, record)
    }

    pub(super) fn new(
        store: Arc<runtime::artifacts::ArtifactStore>,
        renderer: Option<RendererOwner>,
    ) -> Self {
        Self {
            store: Some(store),
            provider: Arc::new(super::browser::run_pages::RuntimePages::new(
                renderer.clone(),
            )),
            renderer,
        }
    }
    /// Catalog availability is independent of helper installation or state.
    pub(super) fn artifact_pages(&self) -> Option<Arc<dyn runtime::artifacts::ArtifactPages>> {
        Some(self.provider.clone())
    }
}
