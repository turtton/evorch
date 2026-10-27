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
        host_inputs_json: "[]".into(),
    };
    conn.execute(
        "INSERT INTO run_contexts (run_id,role,name,parent_run_id,config_json,messages_json,checkpoints_json,terminal_phase,restorable,updated_at_ns,snapshot_revision) VALUES (?1,'worker','worker',NULL,?2,?3,'[]','Done',1,1,0)",
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
fn guarded_reader_proof_commits_once_with_source_revision_and_survives_restart() {
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
        original.provenance.as_ref().unwrap().status,
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
    assert_eq!(
        index
            .index_run_status("run-1")
            .unwrap()
            .unwrap()
            .source
            .target
            .source_revision,
        0
    );
    let guard = legacy_guard(&db.conn, "run-1", 1);
    let sidecar = original.provenance.unwrap();
    let dependencies = crate::repo::thread_index::LegacyDependencies::capture(&db.conn, "run-1")
        .unwrap()
        .unwrap();
    assert_eq!(
        index
            .commit_legacy_provenance(&guard, &sidecar, &dependencies, 10)
            .unwrap(),
        IndexCommitResult::Committed
    );
    let revision = index
        .index_run_status("run-1")
        .unwrap()
        .unwrap()
        .source
        .target
        .source_revision;
    assert!(revision > 0);
    assert_eq!(
        index
            .commit_legacy_provenance(&guard, &sidecar, &dependencies, 10)
            .unwrap(),
        IndexCommitResult::Discarded(CommitRejection::SourceChanged)
    );
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
fn explicit_reindex_repairs_an_already_blocked_missing_sidecar_run() {
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
    let blocked = IndexBlockedReason::Extraction(ExtractionError::ProvenanceUnknown(
        ProvenanceUnknownReason::LegacyUnproven,
    ));
    db.conn
        .execute(
            "UPDATE thread_index_runs SET state='blocked',blocked_reason=?1 WHERE run_id='run-1'",
            [serde_json::to_string(&blocked).unwrap()],
        )
        .unwrap();
    index.discover_batch(20, 20).unwrap();
    assert_eq!(
        index.index_run_status("run-1").unwrap().unwrap().state,
        IndexState::Blocked
    );
    db.conn.execute("UPDATE thread_index_runs SET state='pending',blocked_reason=NULL,work_generation=work_generation+1 WHERE run_id='run-1'", []).unwrap();
    let sidecar = db
        .snapshot_bundle("run-1")
        .unwrap()
        .unwrap()
        .provenance
        .unwrap();
    let guard = legacy_guard(&db.conn, "run-1", 1);
    let dependencies = crate::repo::thread_index::LegacyDependencies::capture(&db.conn, "run-1")
        .unwrap()
        .unwrap();
    assert_eq!(
        index
            .commit_legacy_provenance(&guard, &sidecar, &dependencies, 20)
            .unwrap(),
        IndexCommitResult::Committed
    );
    let status = index.index_run_status("run-1").unwrap().unwrap();
    assert_eq!(status.state, IndexState::Pending);
    assert!(status.source.target.source_revision > 0);
}

fn legacy_guard(conn: &Connection, run: &str, epoch: u64) -> IndexCommitGuard {
    use crate::thread_index::ThreadIndexRepository;
    conn.execute("UPDATE thread_index_lease SET owner_id='test',owner_kind='cli',epoch=?1,expires_at_ns=1000 WHERE id=1", [epoch]).unwrap();
    let row = crate::repo::thread_index::SqliteThreadIndex::new(conn)
        .index_run_status(run)
        .unwrap()
        .unwrap();
    IndexCommitGuard {
        lease: IndexLease {
            owner: IndexOwner {
                id: "test".into(),
                kind: IndexOwnerKind::Cli,
            },
            epoch,
            expires_at_ns: 1000,
        },
        target: row.source.target,
        work_generation: row.work_generation,
        expected_state: IndexState::Pending,
        source_present: true,
        request: None,
    }
}

#[test]
fn reader_proof_cannot_overwrite_a_new_snapshot_or_a_new_lease() {
    for changed_snapshot in [false, true] {
        let db = crate::Database::open_in_memory().unwrap();
        let record = seed(
            &db.conn,
            "run-1",
            Some(json!({"inherited_note":null,"sources":[]})),
        );
        let mut index = crate::repo::thread_index::SqliteThreadIndex::new(&db.conn);
        index.discover_batch(20, 10).unwrap();
        let guard = legacy_guard(&db.conn, "run-1", 1);
        let proof = db
            .snapshot_bundle("run-1")
            .unwrap()
            .unwrap()
            .provenance
            .unwrap();
        let dependencies =
            crate::repo::thread_index::LegacyDependencies::capture(&db.conn, "run-1")
                .unwrap()
                .unwrap();
        let expected = if changed_snapshot {
            crate::repo::run_context::upsert(&db.conn, &record).unwrap();
            CommitRejection::SourceChanged
        } else {
            legacy_guard(&db.conn, "run-1", 2);
            CommitRejection::LeaseLost
        };
        assert_eq!(
            index
                .commit_legacy_provenance(&guard, &proof, &dependencies, 20)
                .unwrap(),
            IndexCommitResult::Discarded(expected)
        );
        if changed_snapshot {
            assert!(matches!(
                db.snapshot_bundle("run-1")
                    .unwrap()
                    .unwrap()
                    .provenance
                    .unwrap()
                    .status,
                ProvenanceStatus::Unknown { .. }
            ));
        } else {
            assert_eq!(
                db.conn
                    .query_row("SELECT count(*) FROM run_context_provenance", [], |row| row
                        .get::<_, u64>(0))
                    .unwrap(),
                0
            );
        }
    }
}

#[test]
fn reader_proof_rejects_changed_binding_and_fork_source_headers() {
    use crate::thread_index::ThreadIndexRepository;
    for change in [
        "UPDATE thread_runs SET depth=1 WHERE run_id='run-1'",
        "UPDATE threads SET fork_point_context_len=4 WHERE thread_id='run-1'",
        "UPDATE run_contexts SET snapshot_revision=1 WHERE run_id='source'",
        "INSERT INTO review_lineage_edges(child_run_id,parent_run_id,created_at_ns) VALUES('run-1','source',10)",
    ] {
        let db = crate::Database::open_in_memory().unwrap();
        seed(
            &db.conn,
            "source",
            Some(json!({"inherited_note":null,"sources":[]})),
        );
        seed(
            &db.conn,
            "run-1",
            Some(json!({"inherited_note":null,"sources":[]})),
        );
        db.conn.execute("UPDATE threads SET fork_point_run_id='source',fork_point_context_len=2,lineage_kind='fork' WHERE thread_id='run-1'",[]).unwrap();
        let mut index = crate::repo::thread_index::SqliteThreadIndex::new(&db.conn);
        index.discover_batch(20, 10).unwrap();
        let guard = legacy_guard(&db.conn, "run-1", 1);
        let proof = db
            .snapshot_bundle("run-1")
            .unwrap()
            .unwrap()
            .provenance
            .unwrap();
        let dependencies =
            crate::repo::thread_index::LegacyDependencies::capture(&db.conn, "run-1")
                .unwrap()
                .unwrap();
        db.conn.execute(change, []).unwrap();
        assert_eq!(
            index
                .commit_legacy_provenance(&guard, &proof, &dependencies, 20)
                .unwrap(),
            IndexCommitResult::Discarded(CommitRejection::SourceChanged)
        );
        assert_eq!(
            index
                .index_run_status("run-1")
                .unwrap()
                .unwrap()
                .source
                .target
                .source_revision,
            0
        );
    }
}

#[test]
fn legacy_dependency_cycles_cannot_materialize_a_proof() {
    let db = crate::Database::open_in_memory().unwrap();
    seed(
        &db.conn,
        "run-1",
        Some(json!({"inherited_note":null,"sources":[]})),
    );
    db.conn
        .execute(
            "UPDATE run_contexts SET parent_run_id='run-1' WHERE run_id='run-1'",
            [],
        )
        .unwrap();
    assert!(
        crate::repo::thread_index::LegacyDependencies::capture(&db.conn, "run-1")
            .unwrap()
            .is_none()
    );
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
