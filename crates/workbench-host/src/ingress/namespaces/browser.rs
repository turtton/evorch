//! Local renderer operations; shared receipts and events contain opaque ids only.
use super::artifacts::local_surface;
use crate::{
    areas::artifacts::{not_found, storage_error},
    ingress::CommandHost,
};
use workbench_protocol::{
    commands::{CommandReply, CommandResult},
    envelope::Provenance,
    errors::WorkbenchError,
    ids::{ProjectId, ThreadId},
    namespaces::{artifacts::ArtifactRef, browser::*},
    queries::{Query, QueryReply},
};
impl CommandHost {
    pub(in crate::ingress) fn browser_command(
        &mut self,
        command: BrowserCommand,
        provenance: &Provenance,
        restart: crate::bootstrap::restart::HostCommandPermit,
    ) -> Result<CommandReply, WorkbenchError> {
        local_surface(provenance)?;
        self.renderer_sync_runs()?;
        let service = self.areas.browser.service()?;
        if let Some(artifact) = service.command_artifact(&command, provenance)? {
            self.renderer_artifact_record(&artifact)?;
        }
        let operation_id = service.command(command, provenance, restart)?;
        Ok(CommandReply::with_result(CommandResult::Browser(
            BrowserResult::Accepted { operation_id },
        )))
    }
    pub(in crate::ingress) fn browser_query(
        &self,
        _query: &Query,
        query: &BrowserQuery,
        provenance: &Provenance,
    ) -> Result<QueryReply, WorkbenchError> {
        local_surface(provenance)?;
        self.renderer_sync_runs()?;
        let service = self.areas.browser.service()?;
        let reply = match query {
            BrowserQuery::ListRendererTabs { offset, limit } => {
                let BrowserReply::Tabs { tabs, .. } = service.query(
                    &BrowserQuery::ListRendererTabs {
                        offset: 0,
                        limit: MAX_RENDERER_TABS as u32,
                    },
                    provenance,
                )?
                else {
                    unreachable!()
                };
                let mut tabs = tabs
                    .into_iter()
                    .filter(|tab| self.renderer_artifact_record(&tab.artifact).is_ok())
                    .collect::<Vec<_>>();
                let start = (*offset as usize).min(tabs.len());
                let end = start.saturating_add(*limit as usize).min(tabs.len());
                let more = end < tabs.len();
                BrowserReply::Tabs {
                    tabs: tabs.drain(start..end).collect(),
                    more,
                }
            }
            BrowserQuery::GetRendererTab { .. } => {
                let reply = service.query(query, provenance)?;
                if let BrowserReply::Tab(tab) = &reply {
                    self.renderer_artifact_record(&tab.artifact)?;
                }
                reply
            }
            BrowserQuery::GetRendererOperation { operation_id } => {
                let reply = service.query(query, provenance)?;
                if let Some(artifact) = service.operation_artifact(operation_id) {
                    self.renderer_artifact_record(&artifact)?;
                }
                reply
            }
            _ => service.query(query, provenance)?,
        };
        Ok(QueryReply::Browser(reply))
    }
    /// Syncs the run tabs and lists the ones that passed the stored-thread and
    /// artifact checks, for a test that cannot see the private state.
    #[cfg(any(test, feature = "test-support"))]
    pub fn renderer_run_tabs(&self) -> Result<Vec<RendererTab>, WorkbenchError> {
        self.renderer_sync_runs()?;
        Ok(self.areas.browser.service()?.run_tabs())
    }
    fn renderer_sync_runs(&self) -> Result<(), WorkbenchError> {
        let service = self.areas.browser.service()?;
        let Some(owner) = self.areas.browser.renderer.as_ref() else {
            return Err(not_found());
        };
        let runs = owner
            .run_tabs
            .lock()
            .unwrap()
            .iter()
            .take(MAX_RENDERER_TABS)
            .map(|(run_id, run)| (run_id.clone(), run.clone()))
            .collect::<Vec<_>>();
        let mut verified = Vec::new();
        for (run_id, run) in runs {
            let Some(record) = &run.artifact else {
                continue;
            };
            // The artifact may come from another run of the same conversation
            // (`inspect_artifact` allows that): the displaying run's thread must be
            // the stored artifact's thread, and `renderer_artifact_record` verifies
            // the stored artifact itself.
            let affiliation = self
                .service
                .runtime()
                .with_storage_reader(|db| {
                    let Some(link) = db.thread_of_run(&run_id)? else {
                        return Ok(None);
                    };
                    if record.thread_id.as_deref() != Some(link.thread_id.as_str()) {
                        return Ok(None);
                    }
                    db.thread(&link.thread_id)
                })
                .map_err(|_| storage_error())?
                .flatten();
            let Some(thread) = affiliation else {
                continue;
            };
            let Some(project) = thread.project_id.and_then(|id| ProjectId::new(id).ok()) else {
                continue;
            };
            let Ok(thread) = ThreadId::new(thread.thread_id) else {
                continue;
            };
            let artifact = ArtifactRef {
                project,
                thread,
                artifact_id: record.artifact_id.clone(),
                digest: record.sha256.clone(),
            };
            if self.renderer_artifact_record(&artifact).is_ok() {
                verified.push((run, artifact));
            }
        }
        service.sync_runs(verified);
        Ok(())
    }
    pub(in crate::ingress) fn browser_command_access(
        command: &BrowserCommand,
    ) -> crate::bootstrap::restart::CommandAccess {
        use crate::bootstrap::restart::CommandAccess;
        match command {
            BrowserCommand::CloseRendererSession { .. }
            | BrowserCommand::CloseRendererTab { .. }
            | BrowserCommand::UnsubscribeRendererFrames { .. } => CommandAccess::Stop,
            _ => CommandAccess::NewWork,
        }
    }
}
