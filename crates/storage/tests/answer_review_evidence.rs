//! Atomic host answers through the additive writer command.
use event_bus::{AnswerOrigin, UserQuestion};
use storage::{Database, ReviewEvidenceRow, Storage, StorageConfig, StorageError};

fn question() -> UserQuestion {
    UserQuestion {
        id: "question-atomic".into(),
        run_id: "run-1".into(),
        root_run_id: "run-1".into(),
        root_name: "root".into(),
        recipient_run_ids: vec![],
        title: "Continue?".into(),
        options: vec![],
        blocking: true,
        answer: None,
        answer_origin: None,
    }
}

fn evidence() -> ReviewEvidenceRow {
    ReviewEvidenceRow {
        id: "answer-evidence".into(),
        root_run_id: "run-1".into(),
        target_run_id: "run-1".into(),
        kind: "user_answer".into(),
        text: "yes".into(),
        in_reply_to_question_id: Some(question().id),
        in_reply_to_title: Some(question().title),
        reference_kind: None,
        reference_context: None,
        created_at_ns: 1,
    }
}

#[test]
fn answer_and_evidence_are_atomic_idempotent_and_first_answer_wins() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("answers.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let db = Database::open(&config).unwrap();
    let handle = storage.handle();
    handle.create_user_question(&question()).unwrap();
    let mut invalid = evidence();
    invalid.in_reply_to_title = Some(format!("sk-{}", "a".repeat(32)));
    assert!(matches!(
        handle.answer_user_question_with_evidence(
            &question().id,
            "yes",
            &AnswerOrigin::Host,
            &[evidence(), invalid]
        ),
        Err(StorageError::SecretDetected {
            entity: "review_evidence",
            ..
        })
    ));
    assert_eq!(
        db.user_question(&question().id).unwrap().unwrap().answer,
        None
    );
    assert!(db.review_evidence_for_target("run-1").unwrap().is_empty());
    for _ in 0..2 {
        handle
            .answer_user_question_with_evidence(
                &question().id,
                "yes",
                &AnswerOrigin::Host,
                &[evidence()],
            )
            .unwrap();
    }
    let mut conflicting = evidence();
    conflicting.id = "conflict".into();
    conflicting.text = "no".into();
    assert!(
        handle
            .answer_user_question_with_evidence(
                &question().id,
                "no",
                &AnswerOrigin::Host,
                &[conflicting]
            )
            .is_err()
    );
    assert_eq!(
        db.user_question(&question().id)
            .unwrap()
            .unwrap()
            .answer
            .as_deref(),
        Some("yes")
    );
    assert_eq!(
        db.review_evidence_for_target("run-1").unwrap(),
        vec![evidence()]
    );
    storage.close();
    let mut suspended = config.clone();
    suspended.hard_limits.max_db_bytes = 1;
    let storage = Storage::open(suspended).unwrap();
    assert!(
        matches!(storage.handle().answer_user_question_with_evidence(&question().id, "yes", &AnswerOrigin::Host, &[evidence()]),
        Err(StorageError::Serialization(message)) if message.contains("writes suspended"))
    );
    assert_eq!(
        db.review_evidence_for_target("run-1").unwrap(),
        vec![evidence()]
    );
}

#[test]
fn concurrent_answers_commit_only_the_winners_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("concurrent.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    storage.handle().create_user_question(&question()).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let answers = ["yes", "no"]
        .map(|answer| {
            let handle = storage.handle();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut row = evidence();
                row.id = answer.into();
                row.text = answer.into();
                barrier.wait();
                (
                    answer,
                    handle.answer_user_question_with_evidence(
                        &question().id,
                        answer,
                        &AnswerOrigin::Host,
                        &[row],
                    ),
                )
            })
        })
        .map(|thread| thread.join().unwrap());
    assert_eq!(
        answers.iter().filter(|(_, result)| result.is_ok()).count(),
        1
    );
    let winner = answers.iter().find(|(_, result)| result.is_ok()).unwrap().0;
    let db = Database::open(&config).unwrap();
    assert_eq!(
        db.user_question(&question().id)
            .unwrap()
            .unwrap()
            .answer
            .as_deref(),
        Some(winner)
    );
    let rows = db.review_evidence_for_target("run-1").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].text, winner);
}
