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

fn rewound_to(thread: &ThreadRecord, point: Option<&ForkPoint>) -> bool {
    thread.is_rewind()
        && thread
            .lineage
            .as_ref()
            .and_then(|lineage| lineage.point.as_ref())
            == point
}

/// The versions of what follows `point` in `id`'s conversation: the conversation
/// that was first rewound there, then every rewind to `point` reached from it
/// (rewinds of rewinds included) in creation order. `id` is always included.
pub fn point_versions(
    threads: &[ThreadRecord],
    id: &ThreadId,
    point: Option<&ForkPoint>,
) -> Vec<ThreadId> {
    let mut anchor = id.clone();
    let mut visited = BTreeSet::new();
    while visited.insert(anchor.clone()) {
        match find(threads, &anchor) {
            Some(thread) if rewound_to(thread, point) => match &thread.parent_thread_id {
                Some(parent) if find(threads, parent).is_some() => anchor = parent.clone(),
                _ => break,
            },
            _ => break,
        }
    }
    let mut members = BTreeSet::from([anchor.clone()]);
    loop {
        let before = members.len();
        for thread in threads {
            if rewound_to(thread, point)
                && thread
                    .parent_thread_id
                    .as_ref()
                    .is_some_and(|parent| members.contains(parent))
            {
                members.insert(thread.id.clone());
            }
        }
        if members.len() == before {
            break;
        }
    }
    let mut versions = vec![anchor.clone()];
    versions.extend(
        threads
            .iter()
            .filter(|thread| thread.id != anchor && members.contains(&thread.id))
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
