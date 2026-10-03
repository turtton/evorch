//! Version groups: a rewind replaces its parent in place, so the sidebar shows
//! one visible version per group while every version stays restorable.

use std::collections::BTreeSet;

use crate::thread::{ForkPoint, ThreadId, ThreadRecord};

fn find<'a>(threads: &'a [ThreadRecord], id: &ThreadId) -> Option<&'a ThreadRecord> {
    threads.iter().find(|thread| &thread.id == id)
}

/// The original conversation a chain of rewinds replaced.
pub fn version_root(threads: &[ThreadRecord], id: &ThreadId) -> ThreadId {
    let mut current = id.clone();
    let mut visited = BTreeSet::new();
    while visited.insert(current.clone()) {
        match find(threads, &current) {
            Some(thread) if thread.is_rewind() => match &thread.parent_thread_id {
                Some(parent) if find(threads, parent).is_some() => current = parent.clone(),
                _ => break,
            },
            _ => break,
        }
    }
    current
}

/// Every version sharing `id`'s group, in creation order.
pub fn version_members<'a>(threads: &'a [ThreadRecord], id: &ThreadId) -> Vec<&'a ThreadRecord> {
    let root = version_root(threads, id);
    threads
        .iter()
        .filter(|thread| version_root(threads, &thread.id) == root)
        .collect()
}

/// The version currently shown for `id`'s group.
pub fn visible_version(threads: &[ThreadRecord], id: &ThreadId) -> Option<ThreadId> {
    version_members(threads, id)
        .into_iter()
        .find(|thread| !thread.superseded)
        .map(|thread| thread.id.clone())
}

/// Sidebar parent: rewinds are hidden behind their group, and a parent group is
/// represented by its visible version. A missing parent is still reported, so an
/// orphan keeps child semantics.
pub fn display_parent(threads: &[ThreadRecord], id: &ThreadId) -> Option<ThreadId> {
    let root = version_root(threads, id);
    let parent = find(threads, &root)?.parent_thread_id.as_ref()?;
    visible_version(threads, parent)
        .filter(|_| find(threads, parent).is_some())
        .or_else(|| Some(parent.clone()))
}

/// Versions branching from `parent` at `point`: the parent itself first, then its
/// rewinds at that point in creation order.
pub fn branch_versions(
    threads: &[ThreadRecord],
    parent: &ThreadId,
    point: Option<&ForkPoint>,
) -> Vec<ThreadId> {
    let mut versions = vec![parent.clone()];
    versions.extend(
        threads
            .iter()
            .filter(|thread| {
                thread.is_rewind()
                    && thread.parent_thread_id.as_ref() == Some(parent)
                    && thread
                        .lineage
                        .as_ref()
                        .and_then(|lineage| lineage.point.as_ref())
                        == point
            })
            .map(|thread| thread.id.clone()),
    );
    versions
}

/// Make `target` the only visible version of its group.
pub fn show_version(threads: &mut [ThreadRecord], target: &ThreadId) {
    let members: BTreeSet<ThreadId> = version_members(threads, target)
        .into_iter()
        .map(|thread| thread.id.clone())
        .collect();
    for thread in threads
        .iter_mut()
        .filter(|thread| members.contains(&thread.id))
    {
        thread.superseded = &thread.id != target;
    }
}

/// Repair groups left with no or several visible versions; the newest wins.
pub fn normalize_versions(threads: &mut [ThreadRecord]) {
    let roots: BTreeSet<ThreadId> = threads
        .iter()
        .map(|thread| version_root(threads, &thread.id))
        .collect();
    for root in roots {
        let members = version_members(threads, &root);
        let visible = members.iter().filter(|thread| !thread.superseded).count();
        if visible == 1 {
            continue;
        }
        let keep = members
            .iter()
            .rev()
            .find(|thread| !thread.superseded)
            .or_else(|| members.last())
            .map(|thread| thread.id.clone());
        if let Some(keep) = keep {
            show_version(threads, &keep);
        }
    }
}
