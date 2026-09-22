use event_bus::UserQuestion;
use storage::{Database, Storage, StorageConfig};

fn fixture() -> (tempfile::TempDir, Storage, Database, StorageConfig) {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("questions.sqlite3"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let database = Database::open(&config).unwrap();
    (dir, storage, database, config)
}

fn question(id: u64) -> UserQuestion {
    UserQuestion {
        id: format!("question-{id}"),
        run_id: format!("run-{id}"),
        root_run_id: format!("run-{id}"),
        root_name: "chat:Worker:thread".into(),
        title: "Choose scope".into(),
        options: vec!["A".into(), "B".into()],
        blocking: true,
        answer: None,
    }
}

#[test]
fn first_answer_wins_and_same_answer_is_idempotent_across_reopen() {
    let (_dir, storage, database, config) = fixture();
    let question = question(1);
    storage.handle().create_user_question(&question).unwrap();
    storage
        .handle()
        .answer_user_question(&question.id, "自由回答")
        .unwrap();
    storage
        .handle()
        .answer_user_question(&question.id, "自由回答")
        .unwrap();
    assert!(
        storage
            .handle()
            .answer_user_question(&question.id, "different")
            .is_err()
    );
    assert!(database.pending_user_questions().unwrap().is_empty());
    drop(storage);
    let reopened = Database::open(&config).unwrap();
    assert_eq!(
        reopened
            .user_question(&question.id)
            .unwrap()
            .unwrap()
            .answer
            .as_deref(),
        Some("自由回答")
    );
}

#[test]
fn question_limits_prevent_invisible_pending_questions() {
    let (_dir, storage, database, _config) = fixture();
    for id in 0..1024 {
        storage
            .handle()
            .create_user_question(&question(id))
            .unwrap();
    }
    assert!(
        storage
            .handle()
            .create_user_question(&question(1024))
            .unwrap_err()
            .to_string()
            .contains("pending question limit")
    );
    assert_eq!(database.pending_user_questions().unwrap().len(), 1024);
    storage
        .handle()
        .answer_user_question("question-0", "A")
        .unwrap();
    storage
        .handle()
        .create_user_question(&question(1024))
        .unwrap();
    assert_eq!(database.pending_user_questions().unwrap().len(), 1024);
}

#[test]
fn per_run_metadata_and_utf8_byte_limits_are_enforced() {
    let (_dir, storage, database, _config) = fixture();
    for id in 0..32 {
        let mut q = question(id);
        q.run_id = "run-1".into();
        storage.handle().create_user_question(&q).unwrap();
    }
    let mut q = question(32);
    q.run_id = "run-1".into();
    assert!(
        storage
            .handle()
            .create_user_question(&q)
            .unwrap_err()
            .to_string()
            .contains("32 per run")
    );
    let mut q = question(99);
    q.title = "界".repeat(683);
    assert!(storage.handle().create_user_question(&q).is_err());
    q.title = "scope".into();
    q.root_name = "x".repeat(1025);
    assert!(storage.handle().create_user_question(&q).is_err());
    assert_eq!(database.pending_user_questions().unwrap().len(), 32);
}

#[test]
fn secret_rejection_leaves_no_question_or_answer_mutation() {
    let (_dir, storage, database, _config) = fixture();
    let secret = "sk-test-evorch-9f8e7d6c5b4a3f2e1d";
    let mut q = question(1);
    q.title = secret.into();
    assert!(storage.handle().create_user_question(&q).is_err());
    assert!(database.user_question(&q.id).unwrap().is_none());
    q.title = "scope".into();
    storage.handle().create_user_question(&q).unwrap();
    assert!(
        storage
            .handle()
            .answer_user_question(&q.id, secret)
            .is_err()
    );
    assert_eq!(database.user_question(&q.id).unwrap().unwrap().answer, None);
    assert_eq!(database.pending_user_questions().unwrap().len(), 1);
}

#[test]
fn inheritance_is_explicit_preserves_provenance_and_cannot_read_unrelated_questions() {
    let (_dir, storage, database, _config) = fixture();
    let original = question(1);
    let unrelated = question(2); // Same chat name does not grant access.
    storage.handle().create_user_question(&original).unwrap();
    storage.handle().create_user_question(&unrelated).unwrap();
    storage
        .handle()
        .bind_user_questions("run-1", "run-3", std::slice::from_ref(&original.id))
        .unwrap();
    assert_eq!(
        database.user_questions_for_run("run-3").unwrap(),
        vec![original.clone()]
    );
    assert_eq!(
        database.user_questions_for_run("run-2").unwrap(),
        vec![unrelated]
    );
    assert!(
        storage
            .handle()
            .bind_user_questions("run-2", "run-3", std::slice::from_ref(&original.id))
            .is_err()
    );
    storage
        .handle()
        .answer_user_question(&original.id, "A")
        .unwrap();
    let inherited = database.user_questions_for_run("run-3").unwrap().remove(0);
    assert_eq!(inherited.run_id, "run-1");
    assert_eq!(inherited.answer.as_deref(), Some("A"));
    assert_eq!(
        database.user_question_recipients(&original.id).unwrap(),
        vec!["run-1", "run-3"]
    );
}

#[test]
fn inherited_questions_share_the_recipient_cap_with_new_questions() {
    let (_dir, storage, database, _config) = fixture();
    for id in 0..32 {
        let question = question(id);
        storage.handle().create_user_question(&question).unwrap();
        storage
            .handle()
            .bind_user_questions(&question.run_id, "run-1000", &[question.id])
            .unwrap();
    }
    let extra = question(99);
    storage.handle().create_user_question(&extra).unwrap();
    assert!(
        storage
            .handle()
            .bind_user_questions("run-99", "run-1000", &[extra.id])
            .is_err()
    );
    let own = question(1000);
    assert!(storage.handle().create_user_question(&own).is_err());
    assert_eq!(
        database.user_questions_for_run("run-1000").unwrap().len(),
        32
    );
    assert!(database.user_question(&own.id).unwrap().is_none());
}

#[test]
fn reserved_continuation_id_survives_restart_before_run_registration() {
    let (_dir, storage, _database, config) = fixture();
    let question = question(1);
    storage.handle().create_user_question(&question).unwrap();
    storage
        .handle()
        .bind_user_questions("run-1", "run-1000", &[question.id])
        .unwrap();
    drop(storage);
    let reopened = Database::open(&config).unwrap();
    assert_eq!(reopened.max_persisted_run_id().unwrap(), 1000);
}
