//! Three-way merge of sidebar states written by processes sharing one file.

use std::collections::BTreeMap;

use crate::{ProjectRecord, SidebarState, ThreadRecord};

impl SidebarState {
    /// Merges what this process holds (`local`) with what another process saved
    /// (`disk`), both derived from `base`, the state this process last read or wrote.
    ///
    /// Records are matched by id. A side that left a record as it was in `base`
    /// follows the other side, including its deletion; a record changed on both
    /// sides keeps `local`'s edits field by field and takes the rest from `disk`.
    /// Run ids are the union, activity time the later one. Selection (`active_thread`,
    /// `selected_project`) always stays `local`'s.
    #[must_use]
    pub fn merge_three_way(base: &Self, local: &Self, disk: &Self) -> Self {
        let mut projects = merge_keyed(
            &base.projects,
            &local.projects,
            &disk.projects,
            |project| project.id.clone(),
            merge_project,
        );
        let threads = merge_keyed(
            &base.threads,
            &local.threads,
            &disk.threads,
            |thread| thread.id.clone(),
            merge_thread,
        );
        // A thread created on one side must not lose its project because the
        // other side deleted it unchanged.
        for thread in &threads {
            if projects
                .iter()
                .any(|project| project.id == thread.project_id)
            {
                continue;
            }
            if let Some(project) = [&local.projects, &disk.projects, &base.projects]
                .into_iter()
                .find_map(|list| list.iter().find(|project| project.id == thread.project_id))
            {
                projects.push(project.clone());
            }
        }
        let selected_project = local
            .selected_project
            .clone()
            .filter(|id| projects.iter().any(|project| &project.id == id));
        let active_thread = local
            .active_thread
            .clone()
            .filter(|id| threads.iter().any(|thread| &thread.id == id));
        Self {
            version: local.version,
            projects,
            selected_project,
            threads,
            active_thread,
        }
    }
}

/// Three-way merge of key-identified records, with the rules of
/// [`SidebarState::merge_three_way`]: an unchanged side follows the other
/// (including its deletion), `merge_changed(base, local, disk)` resolves a
/// record both sides changed, and the result follows `local`'s order followed
/// by the records only `disk` has.
pub fn merge_keyed<T, K>(
    base: &[T],
    local: &[T],
    disk: &[T],
    key: impl Fn(&T) -> K,
    merge_changed: impl Fn(&T, &T, &T) -> T,
) -> Vec<T>
where
    T: Clone + PartialEq,
    K: Ord + Clone,
{
    let base_by_key: BTreeMap<K, &T> = base.iter().map(|item| (key(item), item)).collect();
    let disk_by_key: BTreeMap<K, &T> = disk.iter().map(|item| (key(item), item)).collect();
    let local_keys: std::collections::BTreeSet<K> = local.iter().map(&key).collect();
    let mut merged = Vec::with_capacity(local.len().max(disk.len()));
    for item in local {
        let id = key(item);
        match (base_by_key.get(&id), disk_by_key.get(&id)) {
            (_, Some(disk_item)) if *disk_item == item => merged.push(item.clone()),
            (Some(base_item), Some(disk_item)) => {
                merged.push(if *base_item == item {
                    (*disk_item).clone()
                } else if *base_item == *disk_item {
                    item.clone()
                } else {
                    merge_changed(base_item, item, disk_item)
                });
            }
            // Added here and unknown to the other side, or both sides added it.
            (None, _) => merged.push(item.clone()),
            // The other side deleted it: keep only an edit made here.
            (Some(base_item), None) => {
                if *base_item != item {
                    merged.push(item.clone());
                }
            }
        }
    }
    for item in disk {
        let id = key(item);
        if local_keys.contains(&id) {
            continue;
        }
        match base_by_key.get(&id) {
            // Added by the other side.
            None => merged.push(item.clone()),
            // Deleted here: keep only an edit made by the other side.
            Some(base_item) if *base_item != item => merged.push(item.clone()),
            Some(_) => {}
        }
    }
    merged
}

fn pick<T: Clone + PartialEq>(base: &T, local: &T, disk: &T) -> T {
    if local == base {
        disk.clone()
    } else {
        local.clone()
    }
}

fn merge_project(
    base: &ProjectRecord,
    local: &ProjectRecord,
    disk: &ProjectRecord,
) -> ProjectRecord {
    ProjectRecord {
        id: local.id.clone(),
        name: pick(&base.name, &local.name, &disk.name),
        repo_root: pick(&base.repo_root, &local.repo_root, &disk.repo_root),
        allowed_directories: pick(
            &base.allowed_directories,
            &local.allowed_directories,
            &disk.allowed_directories,
        ),
        trust: pick(&base.trust, &local.trust, &disk.trust),
    }
}

fn merge_thread(base: &ThreadRecord, local: &ThreadRecord, disk: &ThreadRecord) -> ThreadRecord {
    let mut run_ids = local.run_ids.clone();
    run_ids.extend(
        disk.run_ids
            .iter()
            .filter(|run_id| !local.run_ids.contains(run_id))
            .cloned(),
    );
    ThreadRecord {
        id: local.id.clone(),
        project_id: pick(&base.project_id, &local.project_id, &disk.project_id),
        title: pick(&base.title, &local.title, &disk.title),
        title_is_placeholder: pick(
            &base.title_is_placeholder,
            &local.title_is_placeholder,
            &disk.title_is_placeholder,
        ),
        pinned: pick(&base.pinned, &local.pinned, &disk.pinned),
        archived: pick(&base.archived, &local.archived, &disk.archived),
        created_at: pick(&base.created_at, &local.created_at, &disk.created_at),
        updated_at: local.updated_at.max(disk.updated_at),
        run_ids,
        root_run_id: pick(&base.root_run_id, &local.root_run_id, &disk.root_run_id),
        active_root: pick(&base.active_root, &local.active_root, &disk.active_root),
        extra_read_dirs: pick(
            &base.extra_read_dirs,
            &local.extra_read_dirs,
            &disk.extra_read_dirs,
        ),
        model_preference: pick(
            &base.model_preference,
            &local.model_preference,
            &disk.model_preference,
        ),
        chat_role: pick(&base.chat_role, &local.chat_role, &disk.chat_role),
        draft_input: pick(&base.draft_input, &local.draft_input, &disk.draft_input),
        parent_thread_id: pick(
            &base.parent_thread_id,
            &local.parent_thread_id,
            &disk.parent_thread_id,
        ),
        escalation_source_run_id: pick(
            &base.escalation_source_run_id,
            &local.escalation_source_run_id,
            &disk.escalation_source_run_id,
        ),
        lineage: pick(&base.lineage, &local.lineage, &disk.lineage),
        superseded: pick(&base.superseded, &local.superseded, &disk.superseded),
    }
}
