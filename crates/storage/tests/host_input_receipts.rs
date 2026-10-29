//! Host input delivery receipts through the single-writer boundary.

use storage::{
    AssistantReferenceRow, Database, HostInputReceipt, HostInputStatus, HostInputTransition,
    HostInputWrite, ReviewEvidenceRow, RunContextRecord, Storage, StorageConfig, StorageError,
};

fn fixture() -> (tempfile::TempDir, Storage, Database, StorageConfig) {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("receipts.sqlite3"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let database = Database::open(&config).unwrap();
    (dir, storage, database, config)
}

fn receipt(id: &str, run: &str, status: HostInputStatus) -> HostInputReceipt {
    HostInputReceipt {
        prompt_id: id.into(),
        run_id: run.into(),
        root_run_id: run.into(),
        text: format!("message {id}"),
        image_count: 1,
        status,
        reason: None,
        accepted_at_ns: 10,
        status_at_ns: 10,
    }
}

fn evidence(id: &str, run: &str, text: &str) -> ReviewEvidenceRow {
    ReviewEvidenceRow {
        id: id.into(),
        root_run_id: run.into(),
        target_run_id: run.into(),
        kind: "host_chat".into(),
        text: text.into(),
        in_reply_to_question_id: None,
        in_reply_to_title: None,
        reference_kind: None,
        reference_context: None,
        created_at_ns: 1,
    }
}

fn context(run: &str, host_inputs: &str) -> RunContextRecord {
    RunContextRecord {
        run_id: run.into(),
        role: "Worker".into(),
        name: "chat:Worker:t-1".into(),
        parent_run_id: None,
        config_json: "{}".into(),
        messages_json: r#"[{"role":"user","content":[{"type":"text","text":"hi"}]}]"#.into(),
        checkpoints_json: "[]".into(),
        terminal_phase: "Checkpoint".into(),
        restorable: true,
        updated_at_ns: 5,
        host_inputs_json: host_inputs.into(),
    }
}

fn status(database: &Database, id: &str) -> Option<HostInputStatus> {
    database
        .host_input_receipts(&[id.to_string()])
        .unwrap()
        .first()
        .map(|receipt| receipt.status)
}

#[test]
fn provenance_snapshot_and_receipts_commit_or_roll_back_together() {
    use storage::thread_index::*;

    let (_dir, storage, database, _config) = fixture();
    let handle = storage.handle();
    let write = SnapshotWrite {
        record: context("run-bundle", r#"["prompt-bundle"]"#),
        expected_snapshot_revision: None,
        binding: None,
        status: ProvenanceStatus::Verified {
            evidence: ProvenanceEvidence::CompletePositionMap,
        },
        map: ProvenanceMap {
            version: PROVENANCE_FORMAT_VERSION,
            raw_message_count: 1,
            checkpoint_count: 0,
            messages: vec![MessageProvenance {
                raw_index: 0,
                ordinal: 0,
                blocks: vec![BlockProvenance {
                    block_index: 0,
                    origin: BlockOrigin::Native,
                    excluded_reason: None,
                }],
            }],
        },
    };
    let host_inputs = HostInputWrite {
        inserts: vec![receipt(
            "prompt-bundle",
            "run-bundle",
            HostInputStatus::Delivered,
        )],
        ..Default::default()
    };
    let (saved, _) = handle
        .save_snapshot_bundle_with_host_inputs(&write, &host_inputs)
        .unwrap();
    assert_eq!(
        status(&database, "prompt-bundle"),
        Some(HostInputStatus::Delivered)
    );
    assert_eq!(
        database
            .run_context("run-bundle")
            .unwrap()
            .unwrap()
            .host_inputs_json,
        r#"["prompt-bundle"]"#
    );
    let bundle = database.snapshot_bundle("run-bundle").unwrap().unwrap();
    assert_eq!(bundle.snapshot_revision, saved.snapshot_revision);
    assert_eq!(bundle.provenance.unwrap().status, write.status);
    let saved_messages = bundle.record.messages_json;

    // Duplicate receipt insertion fails after snapshot staging: neither the new
    // snapshot revision nor its sidecar may escape the transaction.
    let mut replacement = write.clone();
    replacement.expected_snapshot_revision = Some(saved.snapshot_revision);
    replacement.record.updated_at_ns += 1;
    replacement.record.messages_json = replacement.record.messages_json.replace("hi", "new");
    assert!(
        handle
            .save_snapshot_bundle_with_host_inputs(&replacement, &host_inputs)
            .is_err()
    );
    let bundle = database.snapshot_bundle("run-bundle").unwrap().unwrap();
    assert_eq!(bundle.snapshot_revision, saved.snapshot_revision);
    assert_eq!(bundle.record.messages_json, saved_messages);
    assert_eq!(
        status(&database, "prompt-bundle"),
        Some(HostInputStatus::Delivered)
    );
}

#[test]
fn accepted_input_commits_evidence_reference_and_receipt_together() {
    let (_dir, storage, database, _config) = fixture();
    let handle = storage.handle();
    handle
        .save_assistant_reference(&AssistantReferenceRow {
            reference_id: "reference".into(),
            source_run_id: "run-1".into(),
            turn_end: 1,
            text: "Proceed?".into(),
            consumed: false,
            created_at_ns: 1,
        })
        .unwrap();

    handle
        .append_host_input_accepted(
            &[evidence("e-1", "run-1", "yes")],
            &[],
            Some("reference"),
            &receipt("p-1", "run-1", HostInputStatus::Accepted),
        )
        .unwrap();

    assert_eq!(
        database.host_input_receipts_for_run("run-1").unwrap(),
        vec![receipt("p-1", "run-1", HostInputStatus::Accepted)]
    );
    assert_eq!(
        database.review_evidence_for_target("run-1").unwrap().len(),
        1
    );
    assert!(
        database
            .assistant_reference("reference")
            .unwrap()
            .unwrap()
            .consumed
    );
    assert_eq!(
        handle
            .host_input_receipts(&["p-1".into(), "missing".into()])
            .unwrap(),
        database.host_input_receipts(&["p-1".into()]).unwrap()
    );
}

#[test]
fn rejected_acceptance_leaves_neither_evidence_nor_receipt() {
    let (_dir, storage, database, _config) = fixture();
    let handle = storage.handle();
    let secret = format!("sk-{}", "a".repeat(32));
    // A secret in the receipt text fails the whole transaction.
    let mut secret_receipt = receipt("p-secret", "run-1", HostInputStatus::Accepted);
    secret_receipt.text = secret.clone();
    let error = handle
        .append_host_input_accepted(
            &[evidence("e-1", "run-1", "plain")],
            &[],
            None,
            &secret_receipt,
        )
        .unwrap_err();
    assert!(!error.to_string().contains(&secret));
    // So does oversized text, and a missing or already consumed reference.
    let mut oversized = receipt("p-big", "run-1", HostInputStatus::Accepted);
    oversized.text = "x".repeat(256 * 1024 + 1);
    assert!(
        handle
            .append_host_input_accepted(&[evidence("e-2", "run-1", "plain")], &[], None, &oversized)
            .is_err()
    );
    assert!(
        handle
            .append_host_input_accepted(
                &[evidence("e-3", "run-1", "plain")],
                &[],
                Some("missing-reference"),
                &receipt("p-ref", "run-1", HostInputStatus::Accepted),
            )
            .is_err()
    );
    assert!(
        database
            .host_input_receipts_for_run("run-1")
            .unwrap()
            .is_empty()
    );
    assert!(
        database
            .review_evidence_for_target("run-1")
            .unwrap()
            .is_empty()
    );

    // A duplicate submission ID keeps the first receipt and adds no evidence.
    handle
        .append_host_input_accepted(
            &[evidence("e-4", "run-1", "first")],
            &[],
            None,
            &receipt("p-1", "run-1", HostInputStatus::Accepted),
        )
        .unwrap();
    assert!(
        handle
            .append_host_input_accepted(
                &[evidence("e-5", "run-1", "second")],
                &[],
                None,
                &receipt("p-1", "run-2", HostInputStatus::Accepted),
            )
            .is_err()
    );
    let evidence = database.review_evidence_for_target("run-1").unwrap();
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].text, "first");
    assert_eq!(
        database.host_input_receipts(&["p-1".into()]).unwrap()[0].run_id,
        "run-1"
    );
}

#[test]
fn closed_writer_rejects_acceptance_without_writing() {
    let (_dir, storage, database, _config) = fixture();
    let handle = storage.handle();
    storage.close();
    assert_eq!(
        handle.append_host_input_accepted(
            &[evidence("e-1", "run-1", "plain")],
            &[],
            None,
            &receipt("p-1", "run-1", HostInputStatus::Accepted),
        ),
        Err(StorageError::WriterClosed)
    );
    assert!(
        database
            .host_input_receipts_for_run("run-1")
            .unwrap()
            .is_empty()
    );
    assert!(
        database
            .review_evidence_for_target("run-1")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn suspended_writer_rejects_every_receipt_write_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = StorageConfig {
        db_path: dir.path().join("suspended.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    storage
        .handle()
        .append_host_input_accepted(
            &[],
            &[],
            None,
            &receipt("p-1", "run-1", HostInputStatus::Accepted),
        )
        .unwrap();
    storage.close();
    config.hard_limits.max_db_bytes = 1;
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let delivered = HostInputWrite {
        transitions: vec![HostInputTransition::new(
            "p-1",
            HostInputStatus::Delivered,
            None,
        )],
        ..Default::default()
    };
    let results = [
        handle
            .append_host_input_accepted(
                &[evidence("e-1", "run-1", "plain")],
                &[],
                None,
                &receipt("p-2", "run-1", HostInputStatus::Accepted),
            )
            .map(drop),
        handle
            .upsert_run_context_with_receipts(&context("run-1", r#"["p-1"]"#), &delivered)
            .map(drop),
        handle
            .transition_host_inputs(&delivered.transitions)
            .map(drop),
        handle.mark_host_inputs_unknown("run-1", "test").map(drop),
    ];
    for result in results {
        assert!(
            matches!(&result, Err(StorageError::Serialization(message)) if message.contains("writes suspended")),
            "{result:?}"
        );
    }
    let database = Database::open(&config).unwrap();
    assert_eq!(
        database.host_input_receipts_for_run("run-1").unwrap(),
        vec![receipt("p-1", "run-1", HostInputStatus::Accepted)]
    );
    assert!(database.run_context("run-1").unwrap().is_none());
}

#[test]
fn snapshot_moves_only_accepted_receipts_and_round_trips_host_inputs() {
    let (_dir, storage, database, _config) = fixture();
    let handle = storage.handle();
    for id in ["p-1", "p-2", "p-3"] {
        handle
            .append_host_input_accepted(
                &[],
                &[],
                None,
                &receipt(id, "run-1", HostInputStatus::Accepted),
            )
            .unwrap();
    }
    // An empty list stays empty.
    let (_, applied) = handle
        .upsert_run_context_with_receipts(&context("run-1", "[]"), &HostInputWrite::default())
        .unwrap();
    assert!(applied.is_empty());
    assert_eq!(
        database
            .run_context("run-1")
            .unwrap()
            .unwrap()
            .host_inputs_json,
        "[]"
    );

    // Delivered and dropped commit with the snapshot; an unknown ID is ignored.
    let write = HostInputWrite {
        transitions: vec![
            HostInputTransition::new("p-1", HostInputStatus::Delivered, None),
            HostInputTransition::new("p-2", HostInputStatus::Dropped, Some("run_ended")),
            HostInputTransition::new("missing", HostInputStatus::Delivered, None),
        ],
        ..Default::default()
    };
    let (_, applied) = handle
        .upsert_run_context_with_receipts(&context("run-1", r#"["p-1","p-1",""]"#), &write)
        .unwrap();
    assert_eq!(applied, write.transitions[..2].to_vec());
    // Duplicate and empty IDs are normalized away.
    assert_eq!(
        database
            .run_context("run-1")
            .unwrap()
            .unwrap()
            .host_inputs_json,
        r#"["p-1"]"#
    );
    assert_eq!(status(&database, "p-1"), Some(HostInputStatus::Delivered));
    let dropped = &database.host_input_receipts(&["p-2".into()]).unwrap()[0];
    assert_eq!(
        (dropped.status, dropped.reason.as_deref()),
        (HostInputStatus::Dropped, Some("run_ended"))
    );

    // A final state never changes, and repeating a transition is a no-op.
    let again = HostInputWrite {
        transitions: vec![
            HostInputTransition::new("p-1", HostInputStatus::Delivered, None),
            HostInputTransition::new("p-2", HostInputStatus::Delivered, None),
            HostInputTransition::new("p-3", HostInputStatus::Delivered, None),
        ],
        ..Default::default()
    };
    let (_, applied) = handle
        .upsert_run_context_with_receipts(&context("run-1", r#"["p-1","p-3"]"#), &again)
        .unwrap();
    assert_eq!(applied, vec![again.transitions[2].clone()]);
    assert_eq!(status(&database, "p-2"), Some(HostInputStatus::Dropped));
    assert_eq!(
        database
            .run_context("run-1")
            .unwrap()
            .unwrap()
            .host_inputs_json,
        r#"["p-1","p-3"]"#
    );
}

#[test]
fn failed_snapshot_rolls_back_its_receipt_changes() {
    let (_dir, storage, database, _config) = fixture();
    let handle = storage.handle();
    handle
        .append_host_input_accepted(
            &[],
            &[],
            None,
            &receipt("p-1", "run-1", HostInputStatus::Accepted),
        )
        .unwrap();
    let write = HostInputWrite {
        transitions: vec![HostInputTransition::new(
            "p-1",
            HostInputStatus::Delivered,
            None,
        )],
        ..Default::default()
    };
    // Malformed host inputs and a duplicate insert both fail the transaction.
    assert!(
        handle
            .upsert_run_context_with_receipts(&context("run-1", "not json"), &write)
            .is_err()
    );
    let duplicate = HostInputWrite {
        inserts: vec![receipt("p-1", "run-1", HostInputStatus::Delivered)],
        ..write.clone()
    };
    assert!(
        handle
            .upsert_run_context_with_receipts(&context("run-1", r#"["p-1"]"#), &duplicate)
            .is_err()
    );
    assert_eq!(status(&database, "p-1"), Some(HostInputStatus::Accepted));
    assert!(database.run_context("run-1").unwrap().is_none());
}

#[test]
fn restore_reconciles_leftovers_and_inserts_the_new_prompt_as_delivered() {
    let (_dir, storage, database, _config) = fixture();
    let handle = storage.handle();
    for (id, run) in [
        ("left", "run-1"),
        ("held", "run-1"),
        ("lost", "run-1"),
        ("other", "run-2"),
    ] {
        handle
            .append_host_input_accepted(
                &[],
                &[],
                None,
                &receipt(id, run, HostInputStatus::Accepted),
            )
            .unwrap();
    }
    handle
        .upsert_run_context_with_receipts(
            &context("run-1", r#"["held","lost"]"#),
            &HostInputWrite {
                transitions: vec![
                    HostInputTransition::new("held", HostInputStatus::Delivered, None),
                    HostInputTransition::new("lost", HostInputStatus::Delivered, None),
                ],
                ..Default::default()
            },
        )
        .unwrap();

    // The restored snapshot holds "held" and the new prompt, but not "lost".
    let write = HostInputWrite {
        inserts: vec![receipt("new", "run-1", HostInputStatus::Delivered)],
        reconcile_reason: Some("restored".into()),
        ..Default::default()
    };
    let applied = handle
        .upsert_run_context_with_evidence_and_host_inputs(
            &context("run-1", r#"["held","new"]"#),
            &[evidence("e-new", "run-1", "message new")],
            &[],
            None,
            &write,
        )
        .unwrap();

    let mut changed: Vec<_> = applied.iter().map(|t| t.prompt_id.as_str()).collect();
    changed.sort_unstable();
    assert_eq!(changed, ["left", "lost"]);
    assert_eq!(status(&database, "left"), Some(HostInputStatus::Unknown));
    assert_eq!(status(&database, "lost"), Some(HostInputStatus::Unknown));
    assert_eq!(status(&database, "held"), Some(HostInputStatus::Delivered));
    assert_eq!(status(&database, "new"), Some(HostInputStatus::Delivered));
    assert_eq!(status(&database, "other"), Some(HostInputStatus::Accepted));
    assert_eq!(
        database.review_evidence_for_target("run-1").unwrap().len(),
        1
    );
}

#[test]
fn marking_unknown_changes_only_accepted_receipts_of_the_run() {
    let (_dir, storage, database, _config) = fixture();
    let handle = storage.handle();
    for (id, run) in [("a", "run-1"), ("b", "run-1"), ("c", "run-2")] {
        handle
            .append_host_input_accepted(
                &[],
                &[],
                None,
                &receipt(id, run, HostInputStatus::Accepted),
            )
            .unwrap();
    }
    handle
        .transition_host_inputs(&[HostInputTransition::new(
            "b",
            HostInputStatus::Dropped,
            Some("spawn_failed"),
        )])
        .unwrap();
    assert_eq!(
        handle
            .mark_host_inputs_unknown("run-1", "run_panicked")
            .unwrap(),
        vec!["a".to_string()]
    );
    let a = &database.host_input_receipts(&["a".into()]).unwrap()[0];
    assert_eq!(
        (a.status, a.reason.as_deref()),
        (HostInputStatus::Unknown, Some("run_panicked"))
    );
    assert_eq!(status(&database, "b"), Some(HostInputStatus::Dropped));
    assert_eq!(status(&database, "c"), Some(HostInputStatus::Accepted));
    // Receipts never move back to accepted.
    assert!(
        handle
            .transition_host_inputs(&[HostInputTransition::new(
                "c",
                HostInputStatus::Accepted,
                None
            )])
            .is_err()
    );
}

#[test]
fn withdrawn_is_terminal_and_survives_reopen_and_reconciliation() {
    let (_dir, storage, database, config) = fixture();
    let handle = storage.handle();
    handle
        .append_host_input_accepted(
            &[],
            &[],
            None,
            &receipt("withdraw", "run-1", HostInputStatus::Accepted),
        )
        .unwrap();
    let withdrawn = HostInputTransition::new("withdraw", HostInputStatus::Withdrawn, None);
    assert_eq!(
        handle
            .transition_host_inputs(std::slice::from_ref(&withdrawn))
            .unwrap(),
        [withdrawn]
    );
    assert_eq!(
        status(&database, "withdraw"),
        Some(HostInputStatus::Withdrawn)
    );
    assert!(
        handle
            .transition_host_inputs(&[HostInputTransition::new(
                "withdraw",
                HostInputStatus::Delivered,
                None
            )])
            .unwrap()
            .is_empty()
    );
    let write = HostInputWrite {
        reconcile_reason: Some("restored".into()),
        ..Default::default()
    };
    handle
        .upsert_run_context_with_receipts(&context("run-1", "[]"), &write)
        .unwrap();
    drop(database);
    drop(handle);
    drop(storage);
    let reopened = Database::open(&config).unwrap();
    assert_eq!(
        status(&reopened, "withdraw"),
        Some(HostInputStatus::Withdrawn)
    );
}
