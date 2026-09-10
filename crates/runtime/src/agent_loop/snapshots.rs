use event_bus::{Event, EventKind, SnapshotEvent};
use tokio::sync::OwnedMutexGuard;

use super::LoopState;
use crate::snapshot::WorkspaceSnapshots;

impl LoopState {
    pub(super) async fn snapshot_before_tool(
        &self,
        name: &str,
        call_id: &str,
    ) -> Result<Option<OwnedMutexGuard<WorkspaceSnapshots>>, String> {
        if !self
            .shared
            .executor
            .tool_permissions(name)
            .is_some_and(|permissions| permissions.fs_write)
        {
            return Ok(None);
        }
        let Some(shared) = self.shared.runtime.upgrade() else {
            return Ok(None);
        };
        let Some(service) = shared.snapshots.get() else {
            return Ok(None);
        };
        let root = shared
            .workspaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&self.task.run_id)
            .and_then(|workspace| workspace.worktree_path.clone());
        let mut workspace = service
            .lock(root.as_deref())
            .await
            .map_err(|error| error.to_string())?;
        let owner = self
            .task
            .config
            .name
            .clone()
            .unwrap_or_else(|| self.task.run_id.to_string());
        let (workspace, snapshot) = tokio::task::spawn_blocking(move || {
            let snapshot = workspace.checkpoint(&owner);
            (workspace, snapshot)
        })
        .await
        .map_err(|error| error.to_string())?;
        let snapshot = snapshot.map_err(|error| error.to_string())?;
        self.shared
            .bus
            .emit(Event::new(EventKind::Snapshot(SnapshotEvent {
                run_id: self.task.run_id.to_string(),
                call_id: call_id.to_owned(),
                snapshot_id: snapshot.as_str().to_owned(),
                workspace_root: workspace.root().to_path_buf(),
            })));
        Ok(Some(workspace))
    }
}
