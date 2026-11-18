use storage::thread_index::*;

#[test]
fn guarded_note_and_message_are_distinct_versioned_locators() {
    let note = GuardedReference {
        version: 1,
        run_id: "run-1".into(),
        source_revision: 9,
        source: GuardedSource::WorkingNote { note_seq: 3 },
    };
    let json = serde_json::to_value(&note).unwrap();
    assert_eq!(json["source_kind"], "working_note");
    assert!(json.get("ordinal").is_none());
    assert_eq!(
        serde_json::from_value::<GuardedReference>(json).unwrap(),
        note
    );
    let message = GuardedReference {
        source: GuardedSource::Message {
            snapshot_revision: 4,
            ordinal: 3,
            block_index: 0,
        },
        ..note
    };
    let json = serde_json::to_value(&message).unwrap();
    assert_eq!(json["source_kind"], "message");
    assert!(json.get("note_seq").is_none());
}

#[test]
fn provenance_sidecar_roundtrips_nested_fork_origins_without_body() {
    let origin = BlockOrigin::ForkCopy {
        source: CopySource::Message {
            run_id: "run-1".into(),
            snapshot_revision: 2,
            ordinal: 0,
            block_index: 0,
        },
        original_origin: Box::new(BlockOrigin::ContinuityCopy {
            source: CopySource::FrozenHistory {
                history_source_id: "handoff:run-0".into(),
                record_index: 6,
            },
        }),
        boundary: ForkBoundary {
            run_id: "run-1".into(),
            snapshot_revision: 2,
            context_len: 1,
        },
    };
    let json = serde_json::to_string(&origin).unwrap();
    assert_eq!(serde_json::from_str::<BlockOrigin>(&json).unwrap(), origin);
    assert!(!json.contains("\"text\""));
    assert!(!json.contains("\"body\""));
}

#[test]
fn request_target_gaps_are_not_control_failures_and_settings_match_adr() {
    let result = ReindexResult::CompletedWithGaps {
        request_id: "request-1".into(),
        summary: RequestSummary {
            target_count: 1,
            succeeded: 0,
            deleted_before_submission: 0,
            gaps: vec![RequestGapCount {
                reason: RequestGapReason::Superseded,
                count: 1,
            }],
        },
    };
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["status"], "completed_with_gaps");
    assert_eq!(
        serde_json::from_value::<ReindexResult>(json).unwrap(),
        result
    );
    assert_eq!(DEFAULT_THREAD_INDEX_MAX_BYTES, 536_870_912);
}

#[test]
fn capacity_wait_expired_gap_reason_round_trips_under_its_stable_name() {
    let json = serde_json::to_value(RequestGapReason::CapacityWaitExpired).unwrap();
    assert_eq!(json["reason"], "capacity_wait_expired");
    assert_eq!(
        serde_json::from_value::<RequestGapReason>(json).unwrap(),
        RequestGapReason::CapacityWaitExpired
    );
    let state = RequestTargetState::Gap(RequestGapReason::CapacityWaitExpired);
    let json = serde_json::to_string(&state).unwrap();
    assert_eq!(
        serde_json::from_str::<RequestTargetState>(&json).unwrap(),
        state
    );
}

#[test]
fn opening_the_storage_alone_cancels_a_registration_a_dead_process_left() {
    // Given: a database a previous process left with a registration still in progress
    // (it died between two registration steps), a pending request and a cancelled one
    let temp_dir = tempfile::TempDir::new().expect("temporary directory must be created");
    let path = temp_dir.path().join("storage.db");
    let config = storage::StorageConfig {
        db_path: path.clone(),
        ..storage::StorageConfig::default()
    };
    drop(storage::Database::open(&config).expect("fresh database must open"));
    let connection = rusqlite::Connection::open(&path).expect("migrated database must reopen");
    for (id, state, finished) in [
        ("left-behind", "registering", None),
        ("pending", "pending", None),
        ("cancelled", "cancelled", Some(2_i64)),
    ] {
        connection
            .execute(
                "INSERT INTO thread_index_requests(request_id, include_evicted, state, \
                 created_at_ns, finished_at_ns) VALUES (?1, 0, ?2, 1, ?3)",
                rusqlite::params![id, state, finished],
            )
            .expect("request must insert");
    }
    let state_of = |id: &str| -> String {
        connection
            .query_row(
                "SELECT state FROM thread_index_requests WHERE request_id = ?1",
                [id],
                |row| row.get(0),
            )
            .expect("request row must exist")
    };
    assert_eq!(state_of("left-behind"), "registering");

    // When: the storage starts; nothing submits, registers or cancels anything
    let storage = storage::Storage::open(config).expect("storage must open");

    // Then: the leftover registration is closed as cancelled (the index was never
    // touched), and the other requests are untouched
    assert_eq!(state_of("left-behind"), "cancelled");
    assert_eq!(state_of("pending"), "pending");
    assert_eq!(state_of("cancelled"), "cancelled");
    let status = storage
        .handle()
        .thread_index_request_status("left-behind", 10)
        .expect("a swept registration is observable as cancelled");
    assert_eq!(status.progress, RequestProgress::Cancelled);
    storage.close();
}
