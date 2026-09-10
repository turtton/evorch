use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Mutex, OwnedMutexGuard};

use super::{SnapshotError, SnapshotHistory, SnapshotId, SnapshotStore};

pub struct SnapshotService {
    root: PathBuf,
    directory: PathBuf,
    workspaces: Mutex<BTreeMap<PathBuf, Arc<Mutex<WorkspaceSnapshots>>>>,
}

pub struct WorkspaceSnapshots {
    store: SnapshotStore,
    history: SnapshotHistory,
}

impl SnapshotService {
    pub fn new(root: &Path, directory: &Path) -> Result<Self, SnapshotError> {
        let root = root.canonicalize()?;
        std::fs::create_dir_all(directory)?;
        let directory = directory.canonicalize()?;
        if directory.starts_with(&root) || root.starts_with(&directory) {
            return Err(SnapshotError::InvalidStore);
        }
        Ok(Self { root, directory, workspaces: Mutex::new(BTreeMap::new()) })
    }

    pub async fn lock(&self, root: Option<&Path>) -> Result<OwnedMutexGuard<WorkspaceSnapshots>, SnapshotError> {
        let root = root.unwrap_or(&self.root).to_path_buf();
        let mut workspaces = self.workspaces.lock().await;
        let workspace = match workspaces.get(&root) {
            Some(workspace) => Arc::clone(workspace),
            None => {
                let directory = self.directory.join(format!("workspace-{}", workspaces.len()));
                let open_root = root.clone();
                let store = tokio::task::spawn_blocking(move || SnapshotStore::open(&open_root, &directory))
                    .await.map_err(|error| SnapshotError::Git(error.to_string()))??;
                let workspace = Arc::new(Mutex::new(WorkspaceSnapshots { store, history: SnapshotHistory::default() }));
                workspaces.insert(root, Arc::clone(&workspace));
                workspace
            }
        };
        drop(workspaces);
        Ok(workspace.lock_owned().await)
    }
}

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
        if !changed { return Ok(None); }
        let after = self.store.capture()?;
        self.store.diff(&before, &after).map(Some)
    }

    pub fn root(&self) -> &Path {
        &self.store.root
    }
}
