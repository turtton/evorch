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
                unreadable: None,
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

/// A runtime that accepts every chat and answers the first lookups of what it
/// recorded with an error, then with `recorded`.
struct Flaky {
    failures: std::sync::atomic::AtomicUsize,
    chats: Arc<AtomicUsize>,
    recorded: Vec<storage::HostInputOrigin>,
}

impl crate::model::commands::CommandSink for Flaky {
    fn has_host_input_receipts(&self) -> bool {
        true
    }

    fn host_input_receipts(
        &self,
        _: &[String],
    ) -> Option<Result<Vec<storage::HostInputReceipt>, storage::StorageError>> {
        Some(Ok(Vec::new()))
    }

    fn host_input_origins(
        &self,
        ids: &[String],
    ) -> Option<Result<Vec<storage::HostInputOrigin>, storage::StorageError>> {
        use std::sync::atomic::Ordering;
        if self
            .failures
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Some(Err(storage::StorageError::Io("reader busy".into())));
        }
        let template = &self.recorded[0];
        Some(Ok(ids
            .iter()
            .map(|id| storage::HostInputOrigin {
                prompt_id: id.clone(),
                ..template.clone()
            })
            .collect()))
    }

    fn submit(
        &mut self,
        command: crate::model::commands::WorkbenchCommand,
    ) -> Vec<crate::model::commands::LoopEvent> {
        match command {
            crate::model::commands::WorkbenchCommand::SendChat(chat) => {
                self.chats.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                vec![crate::model::commands::LoopEvent::ChatAccepted {
                    thread_id: chat.thread_id,
                    run_id: "run-1".into(),
                }]
            }
            _ => Vec::new(),
        }
    }
}

fn recorded(skills: &[&str]) -> Vec<storage::HostInputOrigin> {
    vec![storage::HostInputOrigin {
        prompt_id: String::new(),
        text: "use @git-best-practices please".into(),
        skills: skills.iter().map(|skill| (*skill).to_owned()).collect(),
        unreadable: None,
    }]
}

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

fn skill_chips(state: &super::WorkbenchState<crate::fixture::DemoSource>) -> Vec<Vec<String>> {
    state
        .transcript()
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::UserMessage { skills, .. } => Some(skills.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_failed_first_lookup_keeps_the_message_unverified_and_a_later_one_corrects_the_bubble() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state(&dir.path().join("sidebar.json"));
    state.history.clear();
    let chats = Arc::new(AtomicUsize::new(0));
    state.sink = Box::new(Flaky {
        failures: std::sync::atomic::AtomicUsize::new(1),
        chats: chats.clone(),
        recorded: recorded(&["git-best-practices", "also-attached"]),
    });
    state.composer.input = "use @git-best-practices please".into();

    state.submit_composer();

    // Accepted once; the lookup failed, so the composer's copy is shown and flagged.
    assert_eq!(chats.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(state.history[0].origin_unverified);
    assert_eq!(state.history[0].skills, ["git-best-practices"]);
    assert_eq!(skill_chips(&state), [vec!["git-best-practices".to_owned()]]);
    assert!(state.transcript().entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Notice { text } if text.contains("Could not confirm which skills")
    )));
    // The flag is saved, so a restart keeps looking.
    let saved = std::fs::read_to_string(dir.path().join("sidebar.json")).unwrap();
    assert!(saved.contains("origin_unverified"));

    // The next periodic pass reads the record and corrects history and bubble in place.
    state.reconcile_receipts();
    assert!(!state.history[0].origin_unverified);
    assert_eq!(
        state.history[0].skills,
        ["git-best-practices", "also-attached"]
    );
    assert_eq!(
        skill_chips(&state),
        [vec![
            "git-best-practices".to_owned(),
            "also-attached".to_owned()
        ]]
    );
    assert_eq!(
        chats.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "never sent again"
    );
    let saved = std::fs::read_to_string(dir.path().join("sidebar.json")).unwrap();
    assert!(!saved.contains("origin_unverified"));
}

#[test]
fn a_failed_restore_lookup_flags_every_message_until_the_record_can_be_read() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state(&dir.path().join("sidebar.json"));
    state.history[0].status = DeliveryStatus::Delivered;
    let sent = state.history[0].clone();
    let second = UserMessage {
        id: "second".into(),
        ..sent.clone()
    };
    state.history.push(second);
    state.sink = Box::new(Flaky {
        failures: std::sync::atomic::AtomicUsize::new(1),
        chats: Arc::new(AtomicUsize::new(0)),
        recorded: recorded(&["recorded"]),
    });
    state
        .transcripts
        .push_host_input("thread", "old", sent.text.clone(), Vec::new());

    state.apply_recorded_origins(Err(storage::StorageError::Io("db busy".into())));
    assert!(
        state
            .history
            .iter()
            .all(|message| message.origin_unverified)
    );

    // The first repair fails too (the sink's one failure); the second succeeds.
    state.receipt_reconciliation.origin_check = true;
    assert!(!state.repair_unverified_origins());
    assert!(
        state
            .history
            .iter()
            .all(|message| message.origin_unverified)
    );
    assert!(state.repair_unverified_origins());
    assert!(
        state
            .history
            .iter()
            .all(|message| !message.origin_unverified)
    );
    assert_eq!(state.history[0].skills, ["recorded"]);
    assert_eq!(skill_chips(&state), [vec!["recorded".to_owned()]]);
}

#[test]
fn an_unreadable_record_flags_only_its_own_message() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state(&dir.path().join("sidebar.json"));
    let first = state.history[0].clone();
    state.history.push(UserMessage {
        id: "second".into(),
        text: "other words".into(),
        ..first
    });
    let origin = |id: &str, text: &str, skills: &[&str], unreadable: Option<&str>| {
        storage::HostInputOrigin {
            prompt_id: id.into(),
            text: text.into(),
            skills: skills.iter().map(|skill| (*skill).to_owned()).collect(),
            unreadable: unreadable.map(str::to_owned),
        }
    };
    state.apply_recorded_origins(Ok(vec![
        origin(
            "old",
            "retry this",
            &[],
            Some("its attachments are not valid JSON"),
        ),
        origin("second", "other words", &["fine"], None),
    ]));
    assert!(state.history[0].origin_unverified);
    assert!(!state.history[1].origin_unverified);
    assert_eq!(state.history[1].skills, ["fine"]);
}
