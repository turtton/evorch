//! Skills attached to a sent message survive a restart as chips beside its words.
use super::{DeliveryStatus, UserMessage, state, state_for_reload};
use crate::model::transcript::TranscriptEntry;

#[test]
fn a_delivered_message_comes_back_with_its_skill_chips_and_typed_words() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sidebar.json");
    let original = state(&path);
    let message = UserMessage {
        id: "sent".into(),
        status: DeliveryStatus::Delivered,
        text: "run @review please".into(),
        skills: vec!["review".into()],
        resolution: None,
        ..original.history[0].clone()
    };
    // The saved history keeps the names and not the skill bodies.
    let saved = serde_json::to_value(&message).unwrap();
    assert_eq!(saved["skills"], serde_json::json!(["review"]));
    assert_eq!(saved["text"], "run @review please");
    let mut restored = state_for_reload(&path, original.sidebar.clone());
    restored.history = vec![serde_json::from_value(saved).unwrap()];
    let config = storage::StorageConfig {
        db_path: dir.path().join("test.db"),
        ..Default::default()
    };
    restored
        .restore_history(&storage::Database::open(&config).unwrap())
        .unwrap();
    assert!(restored.transcript().entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::UserMessage { text, skills }
            if text == "run @review please" && skills == &["review".to_owned()]
    )));
}

#[test]
fn a_message_saved_before_skills_were_recorded_loads_without_chips() {
    let dir = tempfile::tempdir().unwrap();
    let original = state(&dir.path().join("sidebar.json"));
    let mut saved = serde_json::to_value(&original.history[0]).unwrap();
    saved.as_object_mut().unwrap().remove("skills");
    let loaded: UserMessage = serde_json::from_value(saved).unwrap();
    assert!(loaded.skills.is_empty());
}

/// A sink that recorded `skills` for every submission it is asked about.
struct Recorded(Vec<String>);

impl crate::model::commands::CommandSink for Recorded {
    fn submit(
        &mut self,
        _: crate::model::commands::WorkbenchCommand,
    ) -> Vec<crate::model::commands::LoopEvent> {
        Vec::new()
    }

    fn host_input_origins(
        &self,
        ids: &[String],
    ) -> Option<Result<Vec<storage::HostInputOrigin>, storage::StorageError>> {
        Some(Ok(ids
            .iter()
            .map(|id| storage::HostInputOrigin {
                prompt_id: id.clone(),
                text: "as the runtime recorded it".into(),
                skills: self.0.clone(),
            })
            .collect()))
    }
}

#[test]
fn an_accepted_message_takes_the_runtimes_record_over_the_composers_copy() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state(&dir.path().join("sidebar.json"));
    state.sink = Box::new(Recorded(vec!["recorded".into()]));
    state.history[0].skills = vec!["composer-copy".into()];

    let shown = state.adopt_runtime_origin("old").expect("a record");

    assert_eq!(
        shown,
        ("as the runtime recorded it".into(), vec!["recorded".into()])
    );
    assert_eq!(state.history[0].skills, ["recorded"]);
    assert_eq!(state.history[0].text, "as the runtime recorded it");
    // A sink that records nothing leaves the composer's copy.
    state.sink = Box::new(super::Receipts::default());
    assert!(state.adopt_runtime_origin("old").is_none());
}

#[test]
fn a_restored_bubble_shows_what_the_runtime_recorded_not_the_saved_copy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sidebar.json");
    let original = state(&path);
    let config = storage::StorageConfig {
        db_path: dir.path().join("test.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let mut evidence = storage::ReviewEvidenceRow {
        id: "evidence-1".into(),
        root_run_id: "run-1".into(),
        target_run_id: "run-1".into(),
        kind: "host_chat".into(),
        text: "run @review please".into(),
        in_reply_to_question_id: None,
        in_reply_to_title: None,
        reference_kind: None,
        reference_context: None,
        attachments: Vec::new(),
        created_at_ns: 1,
    };
    evidence.attachments = vec![storage::EvidenceAttachment {
        name: "review".into(),
        scope: "user".into(),
        body: "steps".into(),
    }];
    let receipt = storage::HostInputReceipt {
        prompt_id: "sent".into(),
        run_id: "run-1".into(),
        root_run_id: "run-1".into(),
        text: "run @review please".into(),
        image_count: 0,
        status: storage::HostInputStatus::Delivered,
        reason: None,
        accepted_at_ns: 1,
        status_at_ns: 1,
        evidence_id: Some("evidence-1".into()),
    };
    storage
        .handle()
        .append_host_input_accepted(&[evidence], &[], None, &receipt)
        .unwrap();
    let mut restored = state_for_reload(&path, original.sidebar.clone());
    // The saved copy lost the skills (for example an older file).
    restored.history = vec![UserMessage {
        id: "sent".into(),
        status: DeliveryStatus::Delivered,
        text: "run @review please".into(),
        skills: Vec::new(),
        resolution: None,
        ..original.history[0].clone()
    }];
    restored
        .restore_history(&storage::Database::open(&config).unwrap())
        .unwrap();
    assert_eq!(restored.history[0].skills, ["review"]);
    assert!(restored.transcript().entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::UserMessage { text, skills }
            if text == "run @review please" && skills == &["review".to_owned()]
    )));
}
