use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::Poll;

use event_bus::{Event, EventBus, LifecycleEvent, WorkspaceLockHolder, WorkspaceWait};
use tokio::sync::{Mutex, OwnedMutexGuard, watch};

use super::{SnapshotError, SnapshotHistory, SnapshotId, SnapshotStore};

pub struct SnapshotService {
    root: PathBuf,
    directory: PathBuf,
    workspaces: Mutex<BTreeMap<PathBuf, Arc<WorkspaceEntry>>>,
}

struct WorkspaceEntry {
    root: PathBuf,
    snapshots: Arc<Mutex<WorkspaceSnapshots>>,
    holder: watch::Sender<Option<WorkspaceLockHolder>>,
}

pub struct WorkspaceSnapshots {
    store: SnapshotStore,
    history: SnapshotHistory,
}

/// Carries the observed owner for exactly as long as the workspace lease lives,
/// including when a yielded shell job retains the lease after its tool returns.
pub struct WorkspaceSnapshotGuard {
    guard: OwnedMutexGuard<WorkspaceSnapshots>,
    holder: watch::Sender<Option<WorkspaceLockHolder>>,
}

impl Deref for WorkspaceSnapshotGuard {
    type Target = WorkspaceSnapshots;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl DerefMut for WorkspaceSnapshotGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

impl Drop for WorkspaceSnapshotGuard {
    fn drop(&mut self) {
        // Clear while the mutex is still held. Clearing after unlocking could
        // erase the owner of the next lease.
        self.holder.send_replace(None);
    }
}

struct WorkspaceWaitObservation {
    bus: Arc<EventBus>,
    requester: WorkspaceLockHolder,
    workspace_root: PathBuf,
}

impl WorkspaceWaitObservation {
    fn update(&self, holder: Option<WorkspaceLockHolder>) {
        self.bus
            .emit(Event::new(LifecycleEvent::WorkspaceWaitChanged {
                run_id: self.requester.run_id.clone(),
                call_id: self.requester.call_id.clone(),
                waiting: Some(WorkspaceWait {
                    workspace_root: self.workspace_root.clone(),
                    tool_name: self.requester.tool_name.clone(),
                    command: self.requester.command.clone(),
                    holder,
                }),
            }));
    }
}

impl Drop for WorkspaceWaitObservation {
    fn drop(&mut self) {
        // Also runs when cancellation drops the acquisition future.
        self.bus
            .emit(Event::new(LifecycleEvent::WorkspaceWaitChanged {
                run_id: self.requester.run_id.clone(),
                call_id: self.requester.call_id.clone(),
                waiting: None,
            }));
    }
}

impl SnapshotService {
    pub fn new(root: &Path, directory: &Path) -> Result<Self, SnapshotError> {
        let root = root.canonicalize()?;
        std::fs::create_dir_all(directory)?;
        let directory = directory.canonicalize()?;
        if directory.starts_with(&root) || root.starts_with(&directory) {
            return Err(SnapshotError::InvalidStore);
        }
        Ok(Self {
            root,
            directory,
            workspaces: Mutex::new(BTreeMap::new()),
        })
    }

    pub async fn lock(
        &self,
        root: Option<&Path>,
    ) -> Result<OwnedMutexGuard<WorkspaceSnapshots>, SnapshotError> {
        let workspace = self.workspace(root).await?;
        Ok(Arc::clone(&workspace.snapshots).lock_owned().await)
    }

    pub async fn lock_observed(
        &self,
        root: Option<&Path>,
        requester: WorkspaceLockHolder,
        bus: Arc<EventBus>,
    ) -> Result<WorkspaceSnapshotGuard, SnapshotError> {
        let workspace = self.workspace(root).await?;
        let mut holder = workspace.holder.subscribe();
        // Pending from lock_owned alone can mean cooperative-budget exhaustion,
        // not contention. The immediate acquisition also respects permits
        // already reserved for queued mutex waiters.
        let guard = if let Ok(guard) = Arc::clone(&workspace.snapshots).try_lock_owned() {
            guard
        } else {
            let acquire = Arc::clone(&workspace.snapshots).lock_owned();
            tokio::pin!(acquire);
            // Queue once before publishing the wait and preserve that future
            // across owner updates so observers cannot reorder FIFO acquisition.
            match futures_util::poll!(acquire.as_mut()) {
                Poll::Ready(guard) => guard,
                Poll::Pending => {
                    let waiting = WorkspaceWaitObservation {
                        bus,
                        requester: requester.clone(),
                        workspace_root: workspace.root.clone(),
                    };
                    waiting.update(holder.borrow_and_update().clone());
                    loop {
                        tokio::select! {
                            biased;
                            guard = &mut acquire => break guard,
                            changed = holder.changed() => {
                                // The entry keeps its sender alive during acquisition.
                                if changed.is_ok() {
                                    waiting.update(holder.borrow_and_update().clone());
                                }
                            }
                        }
                    }
                }
            }
        };
        workspace.holder.send_replace(Some(requester));
        Ok(WorkspaceSnapshotGuard {
            guard,
            holder: workspace.holder.clone(),
        })
    }

    async fn workspace(&self, root: Option<&Path>) -> Result<Arc<WorkspaceEntry>, SnapshotError> {
        let root = root.unwrap_or(&self.root).to_path_buf();
        let mut workspaces = self.workspaces.lock().await;
        let workspace = match workspaces.get(&root) {
            Some(workspace) => Arc::clone(workspace),
            None => {
                let directory = self
                    .directory
                    .join(format!("workspace-{}", workspaces.len()));
                let open_root = root.clone();
                let store = tokio::task::spawn_blocking(move || {
                    SnapshotStore::open(&open_root, &directory)
                })
                .await
                .map_err(|error| SnapshotError::Git(error.to_string()))??;
                let workspace = Arc::new(WorkspaceEntry {
                    root: store.root.clone(),
                    snapshots: Arc::new(Mutex::new(WorkspaceSnapshots {
                        store,
                        history: SnapshotHistory::default(),
                    })),
                    holder: watch::channel(None).0,
                });
                workspaces.insert(root, Arc::clone(&workspace));
                workspace
            }
        };
        Ok(workspace)
    }
}

#[cfg(test)]
mod tests;

impl WorkspaceSnapshots {
    pub fn checkpoint(&mut self, owner: &str) -> Result<SnapshotId, SnapshotError> {
        self.history.checkpoint(owner, &mut self.store)
    }

    pub fn restore(&mut self, owner: &str, redo: bool) -> Result<Option<String>, SnapshotError> {
        let before = self.store.capture()?;
        let changed = if redo {
            self.history.redo(owner, &mut self.store)?
        } else {
            self.history.undo(owner, &mut self.store)?
        };
        if !changed {
            return Ok(None);
        }
        let after = self.store.capture()?;
        self.store.diff(&before, &after).map(Some)
    }

    pub fn root(&self) -> &Path {
        &self.store.root
    }
}
