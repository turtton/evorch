//! Read current thread roots only within the host's current project directory.
use crate::{
    lane::{BindingTable, ThreadBinding},
    projects::ProjectDirectory,
};
use runtime::RunId;
use std::collections::BTreeMap;

/// Rebuild on every owner retry. A recipe, archived thread, unknown project or
/// malformed root cannot create a current binding. No schema/sidecar is written.
pub fn load(
    config: &storage::StorageConfig,
    projects: &dyn ProjectDirectory,
) -> Result<BindingTable, storage::StorageError> {
    let db = storage::Database::open(config)?;
    let mut bindings = BTreeMap::new();
    for (project, root) in projects.entries() {
        let mut cursor = None;
        loop {
            let page = db.threads_in_project(&root.to_string_lossy(), cursor.as_ref(), 200)?;
            for thread in page.threads {
                if thread.archived || thread.superseded {
                    continue;
                }
                let Some(run) = thread
                    .root_run_id
                    .as_deref()
                    .and_then(|root| root.parse::<RunId>().ok())
                else {
                    continue;
                };
                if thread.project_root.as_deref() != root.to_str() {
                    continue;
                }
                bindings.insert(
                    thread.thread_id,
                    ThreadBinding {
                        root: run,
                        project_root: Some(root.clone()),
                        goal_project: Some(project.as_str().to_owned()),
                        model_preference: None,
                    },
                );
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
    }
    let current = BindingTable::default();
    current.replace(bindings);
    Ok(current)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::projects::StaticProjects;
    #[test]
    fn all_pages_are_loaded_without_promoting_archived_or_unregistered_threads() {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("bindings.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let root = dir.path().join("allowed");
        let mut rows = (0..201)
            .map(|i| storage::threads::ThreadUpsert {
                thread_id: format!("thread-{i:03}"),
                root_run_id: Some(RunId::new(i + 1).to_string()),
                project_root: Some(root.to_string_lossy().into_owned()),
                updated_at_ns: i as i64,
                ..Default::default()
            })
            .collect::<Vec<_>>();
        rows.extend([
            storage::threads::ThreadUpsert {
                thread_id: "archived".into(),
                root_run_id: Some(RunId::new(300).to_string()),
                project_root: Some(root.to_string_lossy().into_owned()),
                archived: Some(true),
                ..Default::default()
            },
            storage::threads::ThreadUpsert {
                thread_id: "superseded".into(),
                root_run_id: Some(RunId::new(301).to_string()),
                project_root: Some(root.to_string_lossy().into_owned()),
                superseded: Some(true),
                ..Default::default()
            },
            storage::threads::ThreadUpsert {
                thread_id: "bad-root".into(),
                root_run_id: Some("not-a-run-id".into()),
                project_root: Some(root.to_string_lossy().into_owned()),
                ..Default::default()
            },
            storage::threads::ThreadUpsert {
                thread_id: "unregistered".into(),
                root_run_id: Some(RunId::new(302).to_string()),
                project_root: Some(dir.path().join("other").to_string_lossy().into_owned()),
                ..Default::default()
            },
        ]);
        storage.handle().upsert_threads_host(&rows).unwrap();
        let mut projects = StaticProjects::default();
        projects.insert(
            &workbench_protocol::ids::ProjectId::new("allowed").unwrap(),
            root.clone(),
        );
        let current = load(&config, &projects).unwrap();
        for i in 0..201 {
            assert_eq!(
                current.get(&format!("thread-{i:03}")).unwrap().root,
                RunId::new(i + 1)
            );
        }
        for thread in ["archived", "superseded", "bad-root", "unregistered"] {
            assert!(current.get(thread).is_none());
        }
        projects.remove(&workbench_protocol::ids::ProjectId::new("allowed").unwrap());
        assert!(
            load(&config, &projects)
                .unwrap()
                .get("thread-200")
                .is_none()
        );
    }
}
