use std::collections::BTreeMap;

use super::{SnapshotError, SnapshotId, SnapshotStore};

#[derive(Debug, Default)]
struct ThreadHistory {
    undo: Vec<SnapshotId>,
    redo: Vec<SnapshotId>,
}

#[derive(Debug, Default)]
pub struct SnapshotHistory {
    threads: BTreeMap<String, ThreadHistory>,
}

impl SnapshotHistory {
    pub fn checkpoint(
        &mut self,
        thread: &str,
        store: &mut SnapshotStore,
    ) -> Result<SnapshotId, SnapshotError> {
        let snapshot = store.capture()?;
        let history = self.threads.entry(thread.to_owned()).or_default();
        history.undo.push(snapshot.clone());
        history.redo.clear();
        Ok(snapshot)
    }

    pub fn undo(&mut self, thread: &str, store: &mut SnapshotStore) -> Result<bool, SnapshotError> {
        let Some(history) = self.threads.get_mut(thread) else {
            return Ok(false);
        };
        transfer(store, &mut history.undo, &mut history.redo)
    }

    pub fn redo(&mut self, thread: &str, store: &mut SnapshotStore) -> Result<bool, SnapshotError> {
        let Some(history) = self.threads.get_mut(thread) else {
            return Ok(false);
        };
        transfer(store, &mut history.redo, &mut history.undo)
    }
}

fn transfer(
    store: &mut SnapshotStore,
    source: &mut Vec<SnapshotId>,
    destination: &mut Vec<SnapshotId>,
) -> Result<bool, SnapshotError> {
    let Some(target) = source.last() else {
        return Ok(false);
    };
    let current = store.capture()?;
    store.restore(target)?;
    source.pop();
    destination.push(current);
    Ok(true)
}
