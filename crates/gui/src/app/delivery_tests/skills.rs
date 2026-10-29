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
