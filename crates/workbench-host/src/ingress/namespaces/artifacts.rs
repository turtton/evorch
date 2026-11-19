//! Surface and stored thread/project checks precede artifact disk access.
use crate::{
    areas::artifacts::{metadata, not_found, storage_error},
    ingress::CommandHost,
};
use workbench_protocol::{
    commands::{CommandReply, CommandResult},
    envelope::{Actor, Provenance, Surface},
    errors::{ErrorCode, RetryAdvice, WorkbenchError},
    ids::{ProjectId, ThreadId},
    namespaces::artifacts::{
        ArtifactRef, ArtifactsCommand, ArtifactsQuery, ArtifactsReply, ArtifactsResult,
        MAX_ARTIFACT_PAGE,
    },
    queries::{Query, QueryReply},
};

pub(super) fn surface_denied() -> WorkbenchError {
    WorkbenchError::new(
        ErrorCode::PermissionDenied,
        "renderer surface is not permitted",
        RetryAdvice::Never,
    )
}
pub(super) fn local_surface(provenance: &Provenance) -> Result<(), WorkbenchError> {
    provenance.validate().map_err(|_| surface_denied())?;
    if provenance.actor != Actor::Human
        || provenance.client_id.is_none()
        || !matches!(provenance.surface, Surface::Desktop | Surface::Cli)
    {
        return Err(surface_denied());
    }
    Ok(())
}
fn metadata_surface(provenance: &Provenance) -> Result<(), WorkbenchError> {
    provenance.validate().map_err(|_| surface_denied())?;
    if provenance.actor != Actor::Human
        || provenance.client_id.is_none()
        || !matches!(
            provenance.surface,
            Surface::Desktop | Surface::Cli | Surface::Web
        )
    {
        return Err(surface_denied());
    }
    Ok(())
}
impl CommandHost {
    pub(in crate::ingress) fn artifacts_command(
        &mut self,
        command: ArtifactsCommand,
        provenance: &Provenance,
        restart: crate::bootstrap::restart::HostCommandPermit,
    ) -> Result<CommandReply, WorkbenchError> {
        local_surface(provenance)?;
        let (artifact, session, viewport) = match command {
            ArtifactsCommand::OpenPreview {
                artifact,
                session,
                viewport,
            } => (artifact, session, viewport),
            ArtifactsCommand::BindArtifactContent {
                channel_id,
                artifact,
            } => return self.bind_artifact_content(&channel_id, &artifact, provenance),
        };
        let record = self.renderer_artifact_record(&artifact)?;
        let operation_id = self
            .areas
            .browser
            .service()?
            .open_preview(artifact, record, session, viewport, provenance, restart)?;
        Ok(CommandReply::with_result(CommandResult::Artifacts(
            ArtifactsResult::Accepted { operation_id },
        )))
    }
    pub(in crate::ingress) fn artifacts_query(
        &self,
        _query: &Query,
        query: &ArtifactsQuery,
        provenance: &Provenance,
    ) -> Result<QueryReply, WorkbenchError> {
        metadata_surface(provenance)?;
        // Reject independently of whether the artifact, project or thread exists.
        if matches!(query, ArtifactsQuery::ReadContent { .. }) {
            local_surface(provenance)?;
        }
        let reply = match query {
            ArtifactsQuery::List {
                project,
                thread,
                after,
                limit,
            } => {
                self.renderer_artifact_thread(project, thread)?;
                let limit = (*limit as usize).min(MAX_ARTIFACT_PAGE);
                if limit == 0 {
                    return Err(WorkbenchError::new(
                        ErrorCode::InvalidRequest,
                        "artifact page must not be empty",
                        RetryAdvice::Never,
                    ));
                }
                let rows = self
                    .service
                    .runtime()
                    .with_storage_reader(|db| {
                        db.artifact_refs_for_thread(thread.as_str(), after.as_deref(), limit + 1)
                    })
                    .map_err(|_| storage_error())?
                    .ok_or_else(storage_error)?;
                let has_more = rows.len() > limit;
                let mut artifacts = Vec::new();
                let mut last = None;
                for row in rows.into_iter().take(limit) {
                    last = Some(row.artifact_id.clone());
                    let reference = ArtifactRef {
                        project: project.clone(),
                        thread: thread.clone(),
                        artifact_id: row.artifact_id,
                        digest: row.sha256,
                    };
                    // Missing/corrupt records cannot expose either their body or source path.
                    if let Ok(record) = self
                        .areas
                        .artifacts
                        .verified_record(self.service.runtime(), &reference)
                    {
                        artifacts.push(metadata(reference, &record));
                    }
                }
                ArtifactsReply::List {
                    artifacts,
                    next: has_more.then_some(last).flatten(),
                }
            }
            ArtifactsQuery::Get { artifact } => {
                let record = self.renderer_artifact_record(artifact)?;
                ArtifactsReply::Metadata(metadata(artifact.clone(), &record))
            }
            ArtifactsQuery::ReadContent {
                artifact,
                offset,
                max_bytes,
            } => {
                let record = self.renderer_artifact_record(artifact)?;
                let (text, eof) = self
                    .areas
                    .artifacts
                    .read_content(&record, *offset, *max_bytes)?;
                ArtifactsReply::Content {
                    artifact: artifact.clone(),
                    offset: *offset,
                    text,
                    eof,
                }
            }
        };
        Ok(QueryReply::Artifacts(reply))
    }
    pub(super) fn renderer_artifact_thread(
        &self,
        project: &ProjectId,
        thread: &ThreadId,
    ) -> Result<(), WorkbenchError> {
        let root = self.projects.root_of(project).ok_or_else(not_found)?;
        let stored = self
            .service
            .runtime()
            .with_storage_reader(|db| db.thread(thread.as_str()))
            .map_err(|_| storage_error())?
            .flatten()
            .ok_or_else(not_found)?;
        if stored.project_id.as_deref() != Some(project.as_str())
            || stored.project_root.as_deref().map(std::path::Path::new) != Some(root.as_path())
        {
            return Err(not_found());
        }
        Ok(())
    }
    /// Verified like `ReadContent`: the thread and project checks and the stored
    /// record come first; then the channel (which must be waiting for a
    /// binding) takes the image bytes. Every failure after the surface check is
    /// the same denial for a channel, so a caller learns nothing about channels.
    fn bind_artifact_content(
        &self,
        channel: &workbench_protocol::namespaces::browser::RendererId,
        artifact: &ArtifactRef,
        provenance: &Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        let record = self.renderer_artifact_record(artifact)?;
        let service = self.areas.browser.service().map_err(|_| surface_denied())?;
        let plane = self
            .areas
            .browser
            .renderer
            .as_ref()
            .map(|owner| owner.data_plane())
            .ok_or_else(surface_denied)?;
        if !plane.is_pending(channel) {
            return Err(surface_denied());
        }
        let (start, reader) = self.areas.artifacts.image_blob(&record)?;
        service.bind_image(channel, start, reader, provenance)?;
        Ok(CommandReply::with_result(CommandResult::Artifacts(
            ArtifactsResult::Bound {},
        )))
    }
    pub(super) fn renderer_artifact_record(
        &self,
        reference: &ArtifactRef,
    ) -> Result<runtime::artifacts::ArtifactRecord, WorkbenchError> {
        self.renderer_artifact_thread(&reference.project, &reference.thread)?;
        self.areas
            .artifacts
            .verified_record(self.service.runtime(), reference)
    }
    pub(in crate::ingress) fn artifacts_command_access(
        _command: &ArtifactsCommand,
    ) -> crate::bootstrap::restart::CommandAccess {
        crate::bootstrap::restart::CommandAccess::NewWork
    }
}
