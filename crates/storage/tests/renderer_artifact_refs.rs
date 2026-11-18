use storage::{
    Database, StorageConfig,
    artifact_refs::{ArtifactRef, MAX_THREAD_ARTIFACT_COUNT, MAX_THREAD_ARTIFACT_PAGE},
};

fn reference(id: &str, thread: Option<&str>) -> ArtifactRef {
    ArtifactRef {
        artifact_id: id.into(),
        sha256: "shared".into(),
        thread_id: thread.map(str::to_owned),
        run_id: None,
        recorded_at_ns: 7,
    }
}

fn ids(rows: Vec<ArtifactRef>) -> Vec<String> {
    rows.into_iter().map(|row| row.artifact_id).collect()
}

#[test]
fn thread_affiliation_precedes_lookup_pagination_and_count() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(&StorageConfig {
        db_path: dir.path().join("refs.db"),
        ..Default::default()
    })
    .unwrap();
    let rows = [
        reference("a", Some("owned")),
        reference("b", Some("foreign")),
        reference("c", Some("owned")),
        reference("d", None),
        reference("e", Some("owned")),
    ];
    db.gc_insert_refs(&rows).unwrap();
    let gc_before = db.gc_state().unwrap();
    assert_eq!(
        db.artifact_ref_for_thread("owned", "a").unwrap(),
        Some(rows[0].clone())
    );
    assert!(db.artifact_ref_for_thread("owned", "b").unwrap().is_none());
    assert!(db.artifact_ref_for_thread("owned", "d").unwrap().is_none());
    assert!(
        db.artifact_ref_for_thread("' OR 1=1 --", "a")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        ids(db.artifact_refs_for_thread("owned", None, 2).unwrap()),
        ["a", "c"]
    );
    assert_eq!(
        ids(db.artifact_refs_for_thread("owned", Some("c"), 2).unwrap()),
        ["e"]
    );
    assert_eq!(
        ids(db.artifact_refs_for_thread("owned", Some("b"), 2).unwrap()),
        ["c", "e"]
    );
    assert!(
        db.artifact_refs_for_thread("owned", Some("z"), 2)
            .unwrap()
            .is_empty()
    );
    assert!(
        db.artifact_refs_for_thread("missing", None, 2)
            .unwrap()
            .is_empty()
    );
    assert!(
        db.artifact_refs_for_thread("owned", None, 0)
            .unwrap()
            .is_empty()
    );
    assert_eq!(db.artifact_ref_count_for_thread("owned", 10).unwrap(), 3);
    assert_eq!(db.artifact_ref_count_for_thread("owned", 2).unwrap(), 2);
    assert_eq!(db.artifact_ref_count_for_thread("owned", 0).unwrap(), 0);
    assert_eq!(
        db.artifact_ref_count_for_thread("missing", usize::MAX)
            .unwrap(),
        0
    );
    assert_eq!(db.artifact_refs_after(None, 10).unwrap(), rows);
    assert_eq!(db.gc_state().unwrap(), gc_before);
}

#[test]
fn foreign_growth_does_not_move_page_boundaries_and_public_limits_are_capped() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(&StorageConfig {
        db_path: dir.path().join("refs.db"),
        ..Default::default()
    })
    .unwrap();
    db.gc_insert_refs(&[
        reference("a", Some("owned")),
        reference("c", Some("owned")),
        reference("e", Some("owned")),
    ])
    .unwrap();
    let page = db.artifact_refs_for_thread("owned", None, 2).unwrap();
    let next = db.artifact_refs_for_thread("owned", Some("c"), 2).unwrap();
    let others = (0..20_000)
        .map(|n| reference(&format!("b-{n:05}"), Some("foreign")))
        .collect::<Vec<_>>();
    db.gc_insert_refs(&others).unwrap();
    assert_eq!(db.artifact_refs_for_thread("owned", None, 2).unwrap(), page);
    assert_eq!(
        db.artifact_refs_for_thread("owned", Some("c"), 2).unwrap(),
        next
    );
    assert_eq!(
        db.artifact_ref_count_for_thread("owned", usize::MAX)
            .unwrap(),
        3
    );
    assert_eq!(
        db.artifact_refs_for_thread("foreign", None, usize::MAX)
            .unwrap()
            .len(),
        MAX_THREAD_ARTIFACT_PAGE
    );
    assert_eq!(
        db.artifact_ref_count_for_thread("foreign", usize::MAX)
            .unwrap(),
        MAX_THREAD_ARTIFACT_COUNT
    );
}
