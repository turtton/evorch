use super::*;
use crate::{Role, RunConfig, RunStore};
use std::sync::Arc;

struct HangingModel;
#[async_trait::async_trait]
impl crate::AgentModel for HangingModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "hanging".into()
    }
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, crate::RuntimeError> {
        std::future::pending().await
    }
}

fn runtime(config: &storage::StorageConfig, handle: storage::StorageHandle) -> AgentRuntime {
    let bus = Arc::new(event_bus::EventBus::new(64));
    AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        Arc::new(HangingModel),
    )
    .with_run_store(RunStore::open(config, handle).unwrap())
}

#[tokio::test]
async fn offline_answer_is_durable_and_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("answers.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let live = runtime(&config, storage.handle());
    let run = live
        .delegate_chat(
            "root",
            Role::Orchestrator,
            "Task".into(),
            RunConfig::default(),
        )
        .unwrap();
    let question = live
        .request_user_question(run, "Continue?".into(), vec![], true)
        .unwrap();
    live.cancel(run).unwrap();
    let offline = runtime(&config, storage.handle());
    offline.answer_user_question(&question.id, "yes").unwrap();
    offline.answer_user_question(&question.id, "yes").unwrap();
    let store = offline.shared.run_store.get().unwrap();
    let review = crate::run_store::materialize_review_context(store, run).unwrap();
    let answers: Vec<_> = review
        .requests()
        .0
        .into_iter()
        .filter(|r| r.in_reply_to.is_some())
        .collect();
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].text, "yes");
    assert_eq!(answers[0].target_run_id, question.run_id);
    assert_eq!(answers[0].in_reply_to.as_ref().unwrap().title, "Continue?");
}

#[tokio::test]
async fn multiple_recipients_resync_after_commit_without_duplicate_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("answers.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let runtime = runtime(&config, storage.handle());
    let root = runtime
        .delegate_chat(
            "root",
            Role::Orchestrator,
            "Task".into(),
            RunConfig::default(),
        )
        .unwrap();
    let child = runtime.spawn_reserved_child(
        root,
        runtime.reserve_run_id(),
        Role::Worker,
        "subtask",
        RunConfig::default(),
    );
    let question = runtime
        .request_user_question(root, "Continue?".into(), vec![], false)
        .unwrap();
    storage
        .handle()
        .bind_user_questions(
            &root.to_string(),
            &child.to_string(),
            std::slice::from_ref(&question.id),
        )
        .unwrap();
    let independent = runtime
        .delegate_chat(
            "independent",
            Role::Orchestrator,
            "Different task".into(),
            RunConfig::default(),
        )
        .unwrap();
    storage
        .handle()
        .bind_user_questions(
            &root.to_string(),
            &independent.to_string(),
            std::slice::from_ref(&question.id),
        )
        .unwrap();
    let independent_review = runtime.shared.review_runs.lock().unwrap()[&independent].clone();
    // Separate restored contexts exercise every recipient, not only a shared Arc.
    let store = runtime.shared.run_store.get().unwrap();
    let child_review = crate::run_store::materialize_review_context(store, child).unwrap();
    runtime
        .shared
        .review_runs
        .lock()
        .unwrap()
        .insert(child, child_review.clone());
    runtime.answer_user_question(&question.id, "yes").unwrap();
    let root_review = runtime.shared.review_runs.lock().unwrap()[&root].clone();
    for review in [&root_review, &child_review, &independent_review] {
        assert!(
            review
                .requests()
                .0
                .iter()
                .any(|r| r.text == "yes" && r.in_reply_to.is_some())
        );
        // Model a lost post-commit in-memory update (T-M3b).
        review
            .user_requests
            .lock()
            .unwrap()
            .requests
            .retain(|r| r.in_reply_to.is_none());
        review.mark_unsynced();
    }
    runtime.answer_user_question(&question.id, "yes").unwrap();
    for review in [&root_review, &child_review, &independent_review] {
        assert!(review.stamp().synced);
        assert!(
            review
                .requests()
                .0
                .iter()
                .any(|r| r.text == "yes" && r.in_reply_to.is_some())
        );
    }
    let before = root_review.stamp();
    runtime.answer_user_question(&question.id, "yes").unwrap();
    assert_eq!(root_review.stamp(), before);
    assert_eq!(
        store
            .review_evidence_for_lineage(&[root.to_string()])
            .unwrap()
            .iter()
            .filter(|r| r.kind == "user_answer")
            .count(),
        1
    );
    let agent_question = runtime
        .request_user_question(child, "Which option?".into(), vec![], false)
        .unwrap();
    runtime
        .answer_subagent_question(root, &agent_question.id, "agent answer")
        .await
        .unwrap();
    assert!(
        !store
            .review_evidence_for_lineage(&[child.to_string()])
            .unwrap()
            .iter()
            .any(|r| r.text == "agent answer")
    );
    let restored = crate::run_store::materialize_review_context(store, independent).unwrap();
    assert_eq!(restored.root_run_id, independent.to_string());
    assert!(
        restored
            .requests()
            .0
            .iter()
            .any(|r| r.text == "yes" && r.target_run_id == independent.to_string())
    );
    assert_eq!(
        store
            .review_evidence_for_lineage(&[independent.to_string()])
            .unwrap()
            .iter()
            .filter(|r| r.kind == "user_answer")
            .count(),
        1
    );
    runtime.cancel(independent).unwrap();
    runtime.cancel_subtree(root).unwrap();
}

#[tokio::test]
async fn unresolved_lineage_and_suspended_writes_reject_answers_and_trusted_send() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = storage::StorageConfig {
        db_path: dir.path().join("answers.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let live = runtime(&config, storage.handle());
    let root = live
        .delegate_chat(
            "root",
            Role::Orchestrator,
            "Task".into(),
            RunConfig::default(),
        )
        .unwrap();
    let question = live
        .request_user_question(root, "Continue?".into(), vec![], false)
        .unwrap();
    let legacy = UserQuestion {
        id: "legacy-question".into(),
        run_id: "run-999".into(),
        root_run_id: "run-999".into(),
        ..question.clone()
    };
    storage.handle().create_user_question(&legacy).unwrap();
    assert!(live.answer_user_question(&legacy.id, "yes").is_err());
    assert!(
        live.user_question(&legacy.id)
            .unwrap()
            .unwrap()
            .answer
            .is_none()
    );
    live.cancel(root).unwrap();
    storage.close();
    config.hard_limits.max_db_bytes = 1;
    let storage = storage::Storage::open(config.clone()).unwrap();
    let live = runtime(&config, storage.handle());
    assert!(
        live.answer_user_question(&question.id, "yes")
            .unwrap_err()
            .contains("writes suspended")
    );
    assert!(
        live.user_question(&question.id)
            .unwrap()
            .unwrap()
            .answer
            .is_none()
    );
    // An internal live run needs no initial evidence write.
    let run = live.delegate_background(Role::Worker, "internal".into(), RunConfig::default());
    let review = live.shared.review_runs.lock().unwrap()[&run].clone();
    assert!(
        live.capture_pending_reference(run, "reference".into())
            .is_err()
    );
    assert!(live.peek_pending_reference(run).unwrap().is_none());
    let before = review.stamp();
    let requests_before = review.requests().0;
    assert!(!before.synced);
    assert!(live.send_message(run, "yes".into()).is_err());
    let after = review.stamp();
    assert!(!after.synced);
    assert!(after.revision > before.revision);
    assert_eq!(review.requests().0, requests_before);
    assert_eq!(live.follow_up_status(run).unwrap().pending, 0);
    assert!(live.peek_pending_reference(run).unwrap().is_none());
    live.cancel(run).unwrap();
}

#[tokio::test]
async fn failed_post_commit_reflection_is_unsynced_and_retry_repairs_it() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("answers.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let runtime = runtime(&config, storage.handle());
    let run = runtime
        .delegate_chat(
            "root",
            Role::Orchestrator,
            "Task".into(),
            RunConfig::default(),
        )
        .unwrap();
    let question = runtime
        .request_user_question(run, "Continue?".into(), vec![], false)
        .unwrap();
    let review = runtime.shared.review_runs.lock().unwrap()[&run].clone();
    let shared = review.user_requests.clone();
    assert!(
        std::thread::spawn(move || {
            let _guard = shared.lock().unwrap();
            panic!("inject reflection failure");
        })
        .join()
        .is_err()
    );
    assert!(runtime.answer_user_question(&question.id, "yes").is_err());
    assert_eq!(
        runtime
            .user_question(&question.id)
            .unwrap()
            .unwrap()
            .answer
            .as_deref(),
        Some("yes")
    );
    assert!(!review.stamp().synced);
    let version = *runtime.shared.question_version.borrow();
    let mut events = runtime.shared.bus.subscribe();
    runtime.answer_user_question(&question.id, "yes").unwrap();
    assert!(matches!(events.recv().await.unwrap().kind,
        event_bus::EventKind::Tool(ToolEvent::UserQuestionUpdated { question: updated })
        if updated.id == question.id && updated.answer.as_deref() == Some("yes")));
    assert_ne!(*runtime.shared.question_version.borrow(), version);
    assert!(review.stamp().synced);
    assert!(review.requests().0.iter().any(|r| r.text == "yes"));
    // A corrupt durable lineage cannot be hidden by an idempotent retry.
    review.mark_unsynced();
    storage
        .handle()
        .record_review_lineage_edge(&[
            storage::LineageEdge {
                child_run_id: run.to_string(),
                parent_run_id: "run-998".into(),
                created_at_ns: 1,
            },
            storage::LineageEdge {
                child_run_id: "run-998".into(),
                parent_run_id: run.to_string(),
                created_at_ns: 1,
            },
        ])
        .unwrap();
    assert!(runtime.answer_user_question(&question.id, "yes").is_err());
    assert!(!review.stamp().synced);
    runtime.cancel(run).unwrap();
}
