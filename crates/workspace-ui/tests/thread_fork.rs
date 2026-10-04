use serde_json::json;
use workspace_ui::{
    ForkPoint, LineageKind, ProjectId, ThreadId, ThreadLineage, ThreadRecord, branch_versions,
    display_parent, normalize_versions, show_version, visible_version,
};

fn thread(id: &str) -> ThreadRecord {
    ThreadRecord::new(ThreadId::new(id), ProjectId::new("project"), id)
}

fn branched(id: &str, parent: &str, kind: LineageKind, context_len: u64) -> ThreadRecord {
    let mut record = thread(id);
    record.parent_thread_id = Some(ThreadId::new(parent));
    record.lineage = Some(ThreadLineage {
        kind,
        point: Some(ForkPoint {
            run_id: "run-1".into(),
            context_len,
        }),
    });
    record
}

#[test]
fn legacy_thread_migrates_without_lineage() {
    // Given: persisted thread data from before turn forks, including the retired boundary field.
    let value = json!({
        "id": "root", "project_id": "project", "title": "Root",
        "pinned": false, "run_ids": [],
        "branch": null, "worktree_path": null, "fork_event_id": null
    });
    // When: the legacy record is loaded.
    let record: ThreadRecord = serde_json::from_value(value).expect("legacy record");
    // Then: it is a visible root with no branch point.
    assert_eq!(record.parent_thread_id, None);
    assert_eq!(record.lineage, None);
    assert!(!record.superseded);
}

#[test]
fn lineage_survives_serialization() {
    // Given: a hidden rewind version with an explicit turn boundary.
    let mut record = branched("v2", "root", LineageKind::Rewind, 4);
    record.superseded = true;
    // When: the record is saved and loaded.
    let loaded: ThreadRecord =
        serde_json::from_str(&serde_json::to_string(&record).expect("serialize"))
            .expect("deserialize");
    // Then: ancestry, branch kind and boundary are preserved.
    assert_eq!(loaded, record);
}

#[test]
fn rewind_versions_share_one_sidebar_row_and_switch_in_place() {
    // Given: a root rewound at turn 4, then rewound again at turn 2, with a fork of the root.
    let mut root = thread("root");
    root.superseded = true;
    let mut first = branched("v2", "root", LineageKind::Rewind, 4);
    first.superseded = true;
    let latest = branched("v3", "v2", LineageKind::Rewind, 2);
    let fork = branched("fork", "root", LineageKind::Fork, 4);
    let mut threads = vec![root, first, latest, fork];
    // Then: the group shows its newest version and the fork hangs under that version.
    assert_eq!(
        visible_version(&threads, &ThreadId::new("root")),
        Some(ThreadId::new("v3"))
    );
    assert_eq!(display_parent(&threads, &ThreadId::new("v3")), None);
    assert_eq!(
        display_parent(&threads, &ThreadId::new("fork")),
        Some(ThreadId::new("v3"))
    );
    let point = threads[1].lineage.clone().unwrap().point;
    assert_eq!(
        branch_versions(&threads, &ThreadId::new("root"), point.as_ref()),
        vec![ThreadId::new("root"), ThreadId::new("v2")]
    );
    // When: restoring the pre-rewind version.
    show_version(&mut threads, &ThreadId::new("root"));
    // Then: exactly that version is visible and nothing was removed.
    let visible: Vec<_> = threads
        .iter()
        .filter(|thread| !thread.superseded)
        .map(|thread| thread.id.to_string())
        .collect();
    assert_eq!(visible, ["root", "fork"]);
    assert_eq!(threads.len(), 4);
}

#[test]
fn loading_repairs_groups_without_exactly_one_visible_version() {
    // Given: one group with every version hidden and another with two visible versions.
    let mut hidden = thread("a");
    hidden.superseded = true;
    let mut hidden_rewind = branched("a2", "a", LineageKind::Rewind, 2);
    hidden_rewind.superseded = true;
    let both = thread("b");
    let both_rewind = branched("b2", "b", LineageKind::Rewind, 2);
    let mut threads = vec![hidden, hidden_rewind, both, both_rewind];
    // When: versions are normalized on load.
    normalize_versions(&mut threads);
    // Then: the newest version of each group is the only visible one.
    let visible: Vec<_> = threads
        .iter()
        .filter(|thread| !thread.superseded)
        .map(|thread| thread.id.to_string())
        .collect();
    assert_eq!(visible, ["a2", "b2"]);
}
