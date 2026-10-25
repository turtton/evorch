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
