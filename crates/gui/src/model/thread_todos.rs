//! Latest conversation procedure snapshots, shared by live delivery and replay.
use event_bus::ThreadTodoSnapshot;
use std::collections::BTreeMap;

/// Apply a newer list revision, including cleared lists and ownership transfers.
/// A replayed older snapshot must never resurrect a cleared list or move it back.
pub fn apply_snapshot(
    latest: &mut BTreeMap<String, ThreadTodoSnapshot>,
    snapshot: &ThreadTodoSnapshot,
) -> bool {
    if latest
        .values()
        .any(|current| current.list_id == snapshot.list_id && current.revision >= snapshot.revision)
    {
        return false;
    }
    latest.retain(|thread, current| {
        thread == &snapshot.thread_id || current.list_id != snapshot.list_id
    });
    latest.insert(snapshot.thread_id.clone(), snapshot.clone());
    true
}
