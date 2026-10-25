use super::*;
use serde_json::json;

fn seed(conn: &Connection, run: &str, continuity: Option<Value>) -> RunContextRecord {
    let mut config = json!({"parent_run_id":null,"thread_id":run});
    if let Some(value) = continuity {
        config["continuity"] = value;
    }
    let messages = json!([
        {"role":"system","content":[{"type":"text","text":"instructions"}]},
        {"role":"user","content":[{"type":"text","text":"duplicate"}]},
        {"role":"assistant","content":[{"type":"text","text":"done"}]},
        {"role":"user","content":[{"type":"text","text":"duplicate"}]}
    ]);
    let record = RunContextRecord {
        run_id: run.into(),
        role: "worker".into(),
        name: "worker".into(),
        parent_run_id: None,
        config_json: config.to_string(),
        messages_json: messages.to_string(),
        checkpoints_json: "[]".into(),
        terminal_phase: "Done".into(),
        restorable: true,
        updated_at_ns: 1,
    };
    conn.execute(
        "INSERT INTO run_contexts VALUES (?1,'worker','worker',NULL,?2,?3,'[]','Done',1,1,0)",
        rusqlite::params![run, record.config_json, record.messages_json],
    )
    .unwrap();
    conn.execute("INSERT INTO threads(thread_id,root_run_id,created_at_ns,updated_at_ns,source) VALUES (?1,?1,1,1,'runtime')", [run]).unwrap();
    conn.execute("INSERT INTO thread_runs VALUES (?1,?1,?1,0)", [run])
        .unwrap();
    record
}

#[test]
fn explicit_empty_continuity_is_required_and_followup_copy_never_becomes_native() {
    let db = crate::Database::open_in_memory().unwrap();
    let empty = json!({"inherited_note":null,"sources":[]});
    let native = seed(&db.conn, "run-1", Some(empty.clone()));
    let proof = derive_legacy_provenance(&db.conn, &native)
        .unwrap()
        .unwrap();
    assert!(
        proof
            .map
            .messages
            .iter()
            .flat_map(|message| &message.blocks)
            .all(|block| block.origin == BlockOrigin::Native)
    );
    let absent = seed(&db.conn, "run-2", None);
    assert_eq!(
        derive_legacy_provenance(&db.conn, &absent).unwrap(),
        Err(LegacyProvenanceReason::ContinuityMissing)
    );
    let inherited = seed(
        &db.conn,
        "run-3",
        Some(json!({"inherited_note":null,"sources":[{"source":"handoff:run-1"}]})),
    );
    assert_eq!(
        derive_legacy_provenance(&db.conn, &inherited).unwrap(),
        Err(LegacyProvenanceReason::ContinuityBoundaryUnverifiable)
    );
    db.conn
        .execute(
            "INSERT INTO review_lineage_edges VALUES ('run-1','run-2',1)",
            [],
        )
        .unwrap();
    assert_eq!(
        derive_legacy_provenance(&db.conn, &native).unwrap(),
        Err(LegacyProvenanceReason::LineageUnverifiable)
    );
}

#[test]
fn validated_raw_fork_boundary_excludes_prefix_without_renumbering_or_text_matching() {
    let db = crate::Database::open_in_memory().unwrap();
    let empty = json!({"inherited_note":null,"sources":[]});
    seed(&db.conn, "run-1", Some(empty.clone()));
    let mut fork = seed(&db.conn, "run-2", Some(empty));
    db.conn.execute("UPDATE threads SET lineage_kind='rewind',parent_thread_id='run-1',fork_point_run_id='run-1',fork_point_context_len=2 WHERE thread_id='run-2'", []).unwrap();
    // Compaction keeps the raw prefix and does not redefine the ordinal.
    fork.checkpoints_json =
        json!([{"range":[1,3],"summary":{"role":"user","content":[]}}]).to_string();
    let proof = derive_legacy_provenance(&db.conn, &fork).unwrap().unwrap();
    assert!(matches!(
        proof.map.messages[0].blocks[0].origin,
        BlockOrigin::ForkCopy { .. }
    ));
    assert_eq!(proof.map.messages[2].ordinal, 2);
    assert_eq!(proof.map.messages[2].raw_index, 3);
    assert_eq!(proof.map.messages[2].blocks[0].origin, BlockOrigin::Native);
    db.conn
        .execute(
            "UPDATE threads SET fork_point_context_len=999 WHERE thread_id='run-2'",
            [],
        )
        .unwrap();
    assert_eq!(
        derive_legacy_provenance(&db.conn, &fork).unwrap(),
        Err(LegacyProvenanceReason::ForkBoundaryUnverifiable)
    );
    db.conn
        .execute(
            "UPDATE threads SET fork_point_context_len=1 WHERE thread_id='run-2'",
            [],
        )
        .unwrap();
    assert_eq!(
        derive_legacy_provenance(&db.conn, &fork).unwrap(),
        Err(LegacyProvenanceReason::ForkBoundaryUnverifiable)
    );
}

#[test]
fn discovery_commits_proof_once_with_source_revision_and_survives_restart() {
    use crate::thread_index::ThreadIndexRepository;
    let dir = tempfile::tempdir().unwrap();
    let config = crate::StorageConfig {
        db_path: dir.path().join("legacy.db"),
        ..Default::default()
    };
    let db = crate::Database::open(&config).unwrap();
    seed(
        &db.conn,
        "run-1",
        Some(json!({"inherited_note":null,"sources":[]})),
    );
    let original = db.snapshot_bundle("run-1").unwrap().unwrap();
    assert!(matches!(
        original.provenance.unwrap().status,
        ProvenanceStatus::Verified { .. }
    ));
    assert_eq!(
        db.conn
            .query_row("SELECT count(*) FROM run_context_provenance", [], |row| row
                .get::<_, usize>(0))
            .unwrap(),
        0
    );
    let mut index = crate::repo::thread_index::SqliteThreadIndex::new(&db.conn);
    index.discover_batch(20, 10).unwrap();
    let revision = index
        .index_run_status("run-1")
        .unwrap()
        .unwrap()
        .source
        .target
        .source_revision;
    assert!(revision > 0);
    index.discover_batch(20, 20).unwrap();
    assert_eq!(
        index
            .index_run_status("run-1")
            .unwrap()
            .unwrap()
            .source
            .target
            .source_revision,
        revision
    );
    drop(db);
    let reopened = crate::Database::open(&config).unwrap();
    let bundle = reopened.snapshot_bundle("run-1").unwrap().unwrap();
    assert_eq!(bundle.source_revision, revision);
    assert!(extract_snapshot_bundle(&bundle, &secret_guard::SecretRedactor::from_env()).is_ok());
}

#[test]
fn discovery_repairs_an_already_blocked_missing_sidecar_run() {
    use crate::thread_index::ThreadIndexRepository;
    let db = crate::Database::open_in_memory().unwrap();
    let record = seed(
        &db.conn,
        "run-1",
        Some(json!({"inherited_note":null,"sources":[]})),
    );
    let mut index = crate::repo::thread_index::SqliteThreadIndex::new(&db.conn);
    index
        .discover_source(&IndexSourceHeader {
            target: IndexTarget {
                run_id: record.run_id.clone(),
                source_revision: 0,
                extractor_version: 1,
                accounting_version: 1,
            },
            snapshot_revision: 0,
            context_sha256: String::new(),
            note_seq: None,
            provenance_version: None,
            source_updated_at_ns: 1,
            retention_at_ns: 1,
            source_present: true,
        })
        .unwrap();
    db.conn.execute("UPDATE thread_index_runs SET state='blocked',blocked_reason='provenance_unknown' WHERE run_id='run-1'", []).unwrap();
    index.discover_batch(20, 20).unwrap();
    let status = index.index_run_status("run-1").unwrap().unwrap();
    assert_eq!(status.state, IndexState::Pending);
    assert!(status.source.target.source_revision > 0);
}

#[test]
fn newer_unknown_provenance_is_not_promoted_by_legacy_metadata() {
    let db = crate::Database::open_in_memory().unwrap();
    let record = seed(
        &db.conn,
        "run-1",
        Some(json!({"inherited_note":null,"sources":[]})),
    );
    crate::repo::run_context::upsert(&db.conn, &record).unwrap();
    assert_eq!(
        derive_legacy_provenance(&db.conn, &record).unwrap(),
        Err(LegacyProvenanceReason::LineageUnverifiable)
    );
}

#[test]
fn a_materialized_legacy_source_proof_still_proves_a_later_discovered_fork() {
    let db = crate::Database::open_in_memory().unwrap();
    let source = seed(
        &db.conn,
        "run-1",
        Some(json!({"inherited_note":null,"sources":[]})),
    );
    let mut index = crate::repo::thread_index::SqliteThreadIndex::new(&db.conn);
    index.discover_batch(20, 10).unwrap();
    let fork = seed(
        &db.conn,
        "run-2",
        Some(json!({"inherited_note":null,"sources":[]})),
    );
    db.conn.execute("UPDATE threads SET lineage_kind='fork',parent_thread_id='run-1',fork_point_run_id='run-1',fork_point_context_len=2 WHERE thread_id='run-2'", []).unwrap();
    assert!(derive_legacy_provenance(&db.conn, &source).unwrap().is_ok());
    assert!(derive_legacy_provenance(&db.conn, &fork).unwrap().is_ok());
}
