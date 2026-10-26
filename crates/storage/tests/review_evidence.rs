//! Review evidence and lineage persistence through the single-writer boundary.

use storage::{
    AssistantReferenceRow, Database, LineageEdge, ReviewEvidenceRow, RunContextRecord, Storage,
    StorageConfig, StorageError,
};

fn fixture() -> (tempfile::TempDir, Storage, Database, StorageConfig) {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("review.sqlite3"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let database = Database::open(&config).unwrap();
    (dir, storage, database, config)
}

fn assistant_reference(id: &str) -> AssistantReferenceRow {
    AssistantReferenceRow {
        reference_id: id.into(),
        source_run_id: "source".into(),
        turn_end: 3,
        text: "Shall I implement this plan?".into(),
        consumed: false,
        created_at_ns: 5,
    }
}

#[test]
fn assistant_references_are_durable_immutable_and_guarded() {
    let (_dir, storage, database, config) = fixture();
    let handle = storage.handle();
    let row = assistant_reference("reference");
    assert_eq!(handle.assistant_reference("missing").unwrap(), None);
    handle.save_assistant_reference(&row).unwrap();
    handle.save_assistant_reference(&row).unwrap();
    let mut conflicting = row.clone();
    conflicting.text = "A different plan".into();
    assert_eq!(
        handle.save_assistant_reference(&conflicting),
        Err(StorageError::AssistantReferenceConflict)
    );
    assert_eq!(
        database.assistant_reference("reference").unwrap(),
        Some(row.clone())
    );
    for text in [
        "x".repeat(256 * 1024 + 1),
        "sk-test-evorch-9f8e7d6c5b4a3f2e1d".into(),
    ] {
        let mut invalid = assistant_reference("invalid");
        invalid.text = text;
        assert!(handle.save_assistant_reference(&invalid).is_err());
        assert_eq!(database.assistant_reference("invalid").unwrap(), None);
    }
    #[cfg(target_pointer_width = "64")]
    {
        let mut invalid = assistant_reference("invalid");
        invalid.turn_end = usize::MAX;
        assert!(handle.save_assistant_reference(&invalid).is_err());
        assert_eq!(database.assistant_reference("invalid").unwrap(), None);
    }
    let mut consumed = assistant_reference("already-consumed");
    consumed.consumed = true;
    assert_eq!(
        handle.save_assistant_reference(&consumed),
        Err(StorageError::AssistantReferenceUnavailable)
    );
    assert_eq!(
        database.assistant_reference("already-consumed").unwrap(),
        None
    );
    assert!(
        database
            .persisted_run_ids()
            .unwrap()
            .contains(&"source".into())
    );
    storage.close();
    assert_eq!(
        Database::open(&config)
            .unwrap()
            .assistant_reference("reference")
            .unwrap(),
        Some(row)
    );
}

#[test]
fn reference_consumption_and_context_evidence_commit_or_rollback_together() {
    let (_dir, storage, database, _config) = fixture();
    let handle = storage.handle();
    let reference = assistant_reference("reference");
    handle.save_assistant_reference(&reference).unwrap();
    let original = context();
    handle.upsert_run_context(&original).unwrap();
    let mut updated = original.clone();
    updated.name = "approved plan".into();
    let valid = evidence("approval", "child", "root", 10);
    let mut invalid = valid.clone();
    invalid.id = "invalid".into();
    invalid.text = "x".repeat(256 * 1024 + 1);
    let edges = [edge("child", "root")];
    assert!(
        handle
            .upsert_run_context_with_evidence_and_reference_consumption(
                &updated,
                &[valid.clone(), invalid],
                &edges,
                Some("reference")
            )
            .is_err()
    );
    assert_eq!(database.run_context("child").unwrap(), Some(original));
    assert!(
        database
            .review_evidence_for_target("child")
            .unwrap()
            .is_empty()
    );
    assert!(database.review_lineage_edges("child").unwrap().is_empty());
    assert_eq!(
        handle.assistant_reference("reference").unwrap(),
        Some(reference.clone())
    );
    handle
        .upsert_run_context_with_evidence_and_reference_consumption(
            &updated,
            std::slice::from_ref(&valid),
            &edges,
            Some("reference"),
        )
        .unwrap();
    let mut consumed = reference.clone();
    consumed.consumed = true;
    assert_eq!(
        database.assistant_reference("reference").unwrap(),
        Some(consumed.clone())
    );
    handle.save_assistant_reference(&reference).unwrap();
    assert_eq!(
        handle.assistant_reference("reference").unwrap(),
        Some(consumed)
    );
    let mut retry = updated.clone();
    retry.name = "must not overwrite".into();
    for id in ["reference", "missing"] {
        let extra = evidence(id, "child", "root", 20);
        assert_eq!(
            handle.upsert_run_context_with_evidence_and_reference_consumption(
                &retry,
                &[extra],
                &[edge("child", "other")],
                Some(id)
            ),
            Err(StorageError::AssistantReferenceUnavailable)
        );
        assert_eq!(
            database.run_context("child").unwrap(),
            Some(updated.clone())
        );
        assert_eq!(
            database.review_evidence_for_target("child").unwrap(),
            vec![valid.clone()]
        );
        assert_eq!(database.review_lineage_edges("child").unwrap(), edges);
    }
}

#[test]
fn append_only_evidence_consumes_reference_exactly_once_and_rolls_back_failures() {
    let (_dir, storage, database, config) = fixture();
    let handle = storage.handle();
    let row = assistant_reference("reference");
    handle.save_assistant_reference(&row).unwrap();
    let valid = evidence("approval", "child", "root", 10);
    let mut invalid = valid.clone();
    invalid.text = "sk-test-evorch-9f8e7d6c5b4a3f2e1d".into();
    let edges = [edge("child", "root")];
    assert!(
        handle
            .append_review_evidence_with_reference_consumption(
                &[invalid],
                &edges,
                Some("reference")
            )
            .is_err()
    );
    assert_eq!(
        database.assistant_reference("reference").unwrap(),
        Some(row)
    );
    assert!(
        database
            .review_evidence_for_target("child")
            .unwrap()
            .is_empty()
    );
    assert!(database.review_lineage_edges("child").unwrap().is_empty());
    handle
        .append_review_evidence_with_reference_consumption(
            std::slice::from_ref(&valid),
            &edges,
            Some("reference"),
        )
        .unwrap();
    for id in ["reference", "missing"] {
        assert_eq!(
            handle.append_review_evidence_with_reference_consumption(
                &[evidence(id, "child", "root", 20)],
                &[edge("child", "other")],
                Some(id)
            ),
            Err(StorageError::AssistantReferenceUnavailable)
        );
    }
    assert_eq!(
        database.review_evidence_for_target("child").unwrap(),
        vec![valid]
    );
    assert_eq!(database.review_lineage_edges("child").unwrap(), edges);
    storage.close();
    assert!(
        Database::open(&config)
            .unwrap()
            .assistant_reference("reference")
            .unwrap()
            .unwrap()
            .consumed
    );
}

fn evidence(id: &str, target: &str, root: &str, created_at_ns: i64) -> ReviewEvidenceRow {
    ReviewEvidenceRow {
        id: id.into(),
        root_run_id: root.into(),
        target_run_id: target.into(),
        kind: "user_answer".into(),
        text: "Keep the full review context".into(),
        in_reply_to_question_id: Some("question-1".into()),
        in_reply_to_title: Some("Choose scope".into()),
        reference_kind: Some("assistant_turn".into()),
        reference_context: Some("The previous assistant turn".into()),
        created_at_ns,
    }
}

fn edge(child: &str, parent: &str) -> LineageEdge {
    LineageEdge {
        child_run_id: child.into(),
        parent_run_id: parent.into(),
        created_at_ns: 7,
    }
}

fn context() -> RunContextRecord {
    RunContextRecord {
        run_id: "child".into(),
        role: "worker".into(),
        name: "review".into(),
        parent_run_id: Some("root".into()),
        config_json: "{}".into(),
        messages_json: "[]".into(),
        checkpoints_json: "[]".into(),
        terminal_phase: "Done".into(),
        restorable: true,
        updated_at_ns: 10,
        host_inputs_json: "[]".into(),
    }
}

#[test]
fn evidence_is_idempotent_and_queries_preserve_full_rows_across_reopen() {
    let (_dir, storage, database, config) = fixture();
    let first = evidence("a", "child", "root", 10);
    let second = evidence("b", "grandchild", "root", 20);
    let mut unrelated = evidence("c", "other", "elsewhere", 5);
    unrelated.kind = "initial_prompt".into();
    unrelated.in_reply_to_question_id = None;
    unrelated.in_reply_to_title = None;
    unrelated.reference_kind = None;
    unrelated.reference_context = None;
    let mut host_chat = evidence("d", "grandchild", "root", 20);
    host_chat.kind = "host_chat".into();
    let rows = [
        host_chat.clone(),
        second.clone(),
        unrelated.clone(),
        first.clone(),
    ];
    storage.handle().record_review_evidence(&rows).unwrap();
    storage.handle().record_review_evidence(&rows).unwrap();
    let mut conflicting = first.clone();
    conflicting.text = "Must not replace the original".into();
    storage
        .handle()
        .record_review_evidence(&[conflicting])
        .unwrap();
    assert_eq!(
        database.review_evidence_for_target("child").unwrap(),
        vec![first.clone()]
    );
    assert_eq!(
        database.review_evidence_for_target("other").unwrap(),
        vec![unrelated]
    );
    assert!(
        database
            .review_evidence_for_lineage(&[])
            .unwrap()
            .is_empty()
    );
    let expected = vec![first, second, host_chat];
    assert_eq!(database.review_evidence_for_root("root").unwrap(), expected);
    assert_eq!(
        database
            .review_evidence_for_lineage(&["grandchild".into(), "child".into(), "child".into()])
            .unwrap(),
        expected
    );
    storage.close();
    assert_eq!(
        Database::open(&config)
            .unwrap()
            .review_evidence_for_root("root")
            .unwrap(),
        expected
    );
}

#[test]
fn ancestors_follow_all_parents_without_treating_a_diamond_as_a_cycle() {
    let (_dir, storage, database, _config) = fixture();
    let edges = [edge("child", "root"), edge("grandchild", "child")];
    storage.handle().record_review_lineage_edge(&edges).unwrap();
    storage.handle().record_review_lineage_edge(&edges).unwrap();
    assert_eq!(
        database.review_lineage_edges("grandchild").unwrap(),
        vec![edges[1].clone()]
    );
    assert_eq!(
        database.review_lineage_ancestors("grandchild").unwrap(),
        vec!["child", "root"]
    );
    assert!(
        database
            .review_lineage_ancestors("missing")
            .unwrap()
            .is_empty()
    );
    assert!(
        database
            .review_lineage_ancestors("root")
            .unwrap()
            .is_empty()
    );
    storage
        .handle()
        .record_review_lineage_edge(&[edge("grandchild", "sibling"), edge("sibling", "root")])
        .unwrap();
    assert_eq!(
        database.review_lineage_ancestors("grandchild").unwrap(),
        vec!["child", "root", "sibling"]
    );
}

#[test]
fn ancestors_fail_closed_on_cycles() {
    let (_dir, storage, database, _config) = fixture();
    storage
        .handle()
        .record_review_lineage_edge(&[
            edge("child", "root"),
            edge("root", "child"),
            edge("self", "self"),
        ])
        .unwrap();
    for run_id in ["child", "root", "self"] {
        assert!(matches!(
            database.review_lineage_ancestors(run_id),
            Err(StorageError::Serialization(_))
        ));
    }
}

#[test]
fn ancestors_allow_64_edges_and_reject_longer_paths_including_shared_ancestors() {
    let (_dir, storage, database, _config) = fixture();
    let mut edges = (1..=65)
        .map(|id| edge(&format!("run-{id:02}"), &format!("run-{:02}", id - 1)))
        .collect::<Vec<_>>();
    // Shortcuts cache shared ancestors first; deeper routes must still check their remaining height.
    edges.push(edge("run-65", "run-00"));
    edges.push(edge("run-65", "run-01"));
    storage.handle().record_review_lineage_edge(&edges).unwrap();
    let expected = (0..64).map(|id| format!("run-{id:02}")).collect::<Vec<_>>();
    assert_eq!(
        database.review_lineage_ancestors("run-64").unwrap(),
        expected
    );
    assert!(matches!(
        database.review_lineage_ancestors("run-65"),
        Err(StorageError::Serialization(_))
    ));
}

#[test]
fn each_free_text_field_is_bounded_in_bytes_and_never_truncated() {
    let (_dir, storage, database, _config) = fixture();
    let exact_limit = "界".repeat((256 * 1024) / 3) + "x";
    assert_eq!(exact_limit.len(), 256 * 1024);
    let mut row = evidence("limit", "child", "root", 1);
    row.text = exact_limit.clone();
    row.in_reply_to_title = Some(exact_limit.clone());
    row.reference_context = Some(exact_limit.clone());
    storage
        .handle()
        .record_review_evidence(&[row.clone()])
        .unwrap();
    assert_eq!(
        database.review_evidence_for_target("child").unwrap(),
        vec![row]
    );
    for field in ["text", "in_reply_to_title", "reference_context"] {
        let mut invalid = evidence(field, "invalid", "root", 2);
        let oversize = exact_limit.clone() + "x";
        match field {
            "text" => invalid.text = oversize,
            "in_reply_to_title" => invalid.in_reply_to_title = Some(oversize),
            _ => invalid.reference_context = Some(oversize),
        }
        assert!(matches!(
            storage.handle().record_review_evidence(&[invalid]),
            Err(StorageError::Serialization(_))
        ));
    }
    assert!(
        database
            .review_evidence_for_target("invalid")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn secret_guard_checks_every_free_text_field_and_rolls_back_the_batch() {
    let (_dir, storage, database, _config) = fixture();
    let secret = format!("sk-{}", "a".repeat(32));
    for field in ["text", "in_reply_to_title", "reference_context"] {
        let mut invalid = evidence(field, "child", "root", 2);
        match field {
            "text" => invalid.text = secret.clone(),
            "in_reply_to_title" => invalid.in_reply_to_title = Some(secret.clone()),
            _ => invalid.reference_context = Some(secret.clone()),
        }
        let result = storage
            .handle()
            .record_review_evidence(&[evidence("valid", "child", "root", 1), invalid]);
        let error = result.unwrap_err();
        assert!(
            matches!(error, StorageError::SecretDetected { entity: "review_evidence", field: rejected, .. } if rejected == field)
        );
        assert!(!error.to_string().contains(&secret));
        assert!(
            database
                .review_evidence_for_root("root")
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn persisted_ids_include_evidence_roots_targets_and_both_edge_endpoints() {
    let (_dir, storage, database, _config) = fixture();
    storage
        .handle()
        .record_review_evidence(&[evidence("a", "target-only", "root-only", 1)])
        .unwrap();
    storage
        .handle()
        .record_review_lineage_edge(&[edge("child-only", "parent-only")])
        .unwrap();
    assert_eq!(
        database.persisted_run_ids().unwrap(),
        vec!["child-only", "parent-only", "root-only", "target-only"]
    );
}

#[test]
fn context_evidence_and_edges_commit_together_or_roll_back_on_evidence_failure() {
    let (_dir, storage, database, config) = fixture();
    let original = context();
    let edges = [edge("child", "root")];
    let valid = evidence("valid", "child", "root", 1);
    let mut invalid = evidence("invalid", "child", "root", 2);
    invalid.text = format!("sk-{}", "a".repeat(32));
    let handle = storage.handle();
    // A new snapshot is not left behind by a rejected evidence batch.
    assert!(
        handle
            .upsert_run_context_with_evidence(&original, &[valid.clone(), invalid.clone()], &edges)
            .is_err()
    );
    assert_eq!(database.run_context("child").unwrap(), None);
    assert!(
        database
            .review_evidence_for_root("root")
            .unwrap()
            .is_empty()
    );
    assert!(database.review_lineage_edges("child").unwrap().is_empty());
    // Nor may a failed update replace an existing snapshot.
    handle.upsert_run_context(&original).unwrap();
    let updated = RunContextRecord {
        name: "updated".into(),
        ..original.clone()
    };
    assert!(
        handle
            .upsert_run_context_with_evidence(&updated, &[valid.clone(), invalid], &edges)
            .is_err()
    );
    assert_eq!(database.run_context("child").unwrap(), Some(original));
    handle
        .upsert_run_context_with_evidence(&updated, std::slice::from_ref(&valid), &edges)
        .unwrap();
    storage.close();
    let reopened = Database::open(&config).unwrap();
    assert_eq!(reopened.run_context("child").unwrap(), Some(updated));
    assert_eq!(
        reopened.review_evidence_for_target("child").unwrap(),
        vec![valid]
    );
    assert_eq!(reopened.review_lineage_edges("child").unwrap(), edges);
}

#[test]
fn edge_sql_failure_rolls_back_both_edge_batches_and_combined_context_writes() {
    let (_dir, storage, database, config) = fixture();
    // Install fault injection only while the single writer is closed.
    storage.close();
    let connection = rusqlite::Connection::open(&config.db_path).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_test_edge BEFORE INSERT ON review_lineage_edges
         WHEN NEW.parent_run_id = 'reject'
         BEGIN SELECT RAISE(ABORT, 'injected edge failure'); END;",
        )
        .unwrap();
    drop(connection);
    let storage = Storage::open(config).unwrap();
    let handle = storage.handle();
    let edges = [edge("child", "root"), edge("child", "reject")];
    assert!(matches!(
        handle.record_review_lineage_edge(&edges),
        Err(StorageError::Sqlite(_))
    ));
    assert!(database.review_lineage_edges("child").unwrap().is_empty());
    assert!(matches!(
        handle.upsert_run_context_with_evidence(
            &context(),
            &[evidence("a", "child", "root", 1)],
            &edges,
        ),
        Err(StorageError::Sqlite(_))
    ));
    assert!(database.persisted_run_ids().unwrap().is_empty());
}

#[test]
fn suspended_writer_rejects_all_review_commands_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = StorageConfig {
        db_path: dir.path().join("suspended.db"),
        ..Default::default()
    };
    let reference = assistant_reference("pending");
    let storage = Storage::open(config.clone()).unwrap();
    storage
        .handle()
        .save_assistant_reference(&reference)
        .unwrap();
    storage.close();
    config.hard_limits.max_db_bytes = 1;
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let rows = [evidence("a", "child", "root", 1)];
    let edges = [edge("child", "root")];
    for result in [
        handle.record_review_evidence(&rows),
        handle.record_review_lineage_edge(&edges),
        handle.upsert_run_context_with_evidence(&context(), &rows, &edges),
        handle.save_assistant_reference(&assistant_reference("new")),
        handle.append_review_evidence_with_reference_consumption(&rows, &edges, Some("pending")),
        handle.upsert_run_context_with_evidence_and_reference_consumption(
            &context(),
            &rows,
            &edges,
            Some("pending"),
        ),
    ] {
        assert!(
            matches!(result, Err(StorageError::Serialization(message)) if message.contains("writes suspended"))
        );
    }
    let database = Database::open(&config).unwrap();
    assert_eq!(
        database.assistant_reference("pending").unwrap(),
        Some(reference)
    );
    assert_eq!(database.assistant_reference("new").unwrap(), None);
    assert!(database.run_context("child").unwrap().is_none());
    assert!(
        database
            .review_evidence_for_target("child")
            .unwrap()
            .is_empty()
    );
    assert!(database.review_lineage_edges("child").unwrap().is_empty());
    assert_eq!(database.persisted_run_ids().unwrap(), vec!["source"]);
}

#[test]
fn evidence_with_edges_rolls_back_both_when_evidence_is_rejected() {
    let (_dir, storage, database, _config) = fixture();
    for text in ["x".repeat(256 * 1024 + 1), format!("sk-{}", "a".repeat(32))] {
        let mut rejected = evidence("rejected", "child", "root", 2);
        rejected.text = text;
        assert!(
            storage
                .handle()
                .record_review_evidence_with_edges(
                    &[evidence("valid", "child", "root", 1), rejected],
                    &[edge("child", "root")],
                )
                .is_err()
        );
        assert!(
            database
                .review_evidence_for_target("child")
                .unwrap()
                .is_empty()
        );
        assert!(database.review_lineage_edges("child").unwrap().is_empty());
    }
    let rows = [evidence("valid", "child", "root", 1)];
    let edges = [edge("child", "root")];
    for _ in 0..2 {
        storage
            .handle()
            .record_review_evidence_with_edges(&rows, &edges)
            .unwrap();
    }
    assert_eq!(database.review_evidence_for_target("child").unwrap(), rows);
    assert_eq!(database.review_lineage_edges("child").unwrap(), edges);
}
