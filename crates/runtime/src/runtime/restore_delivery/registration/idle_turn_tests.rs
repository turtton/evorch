use super::*;
use crate::agent_loop::LoopState;
use futures_util::poll;
use providers::{ContentBlock, Message};
use serde_json::{Value, json};

const OUTPUT: &str = "saved reviewer output";

struct Fixture {
    runtime: AgentRuntime,
    parent: LoopState,
    child: RunId,
    cancel: watch::Sender<RunInterrupt>,
    _storage: storage::Storage,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn restore(question: Option<bool>) -> Self {
        let (cancel, cancel_rx) = watch::channel(RunInterrupt::None);
        let (runtime, mut parent) =
            crate::agent_loop::delegate_cleanup_tests::started_fixture(cancel_rx).await;
        parent.transition(AgentRunPhase::Running, None).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: directory.path().join("restore.sqlite3"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let runtime =
            runtime.with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        let caller = parent.caller_run_id();
        let config = RunConfig {
            interactive: true,
            keep_alive: true,
            ..Default::default()
        };
        let child = runtime
            .delegate_background_as_child(caller, Role::Reviewer, "review", config.clone())
            .unwrap();
        if let Some(blocking) = question {
            runtime
                .request_user_question(child, "saved question".into(), vec![], blocking)
                .unwrap();
        }
        runtime.cancel(child).unwrap();
        runtime.wait(child).await.unwrap();
        let old = runtime.run_output(caller, child).unwrap();
        runtime.observe_completion(old.completion_notice.as_ref().unwrap());

        // Model a saved, completed assistant turn. Registration must resume only
        // this history: the synthetic trigger is not new input, and the blocked
        // model cannot supply the output asserted by these tests.
        // The cancelled child may have saved before it stopped; resume from that revision.
        let revision = runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .restore_bundle(child)
            .unwrap()
            .map_or(0, |bundle| bundle.snapshot_revision);
        let restored = RestoredState {
            provenance: Default::default(),
            snapshot_revision: crate::restore::revision::SnapshotRevision::known(revision),
            admission_reservation: None,
            turn: None,
            messages: vec![
                Message {
                    role: providers::Role::User,
                    content: vec![ContentBlock::Text {
                        text: "review".into(),
                    }],
                },
                Message {
                    role: providers::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: OUTPUT.into(),
                    }],
                },
            ],
            continuity: Default::default(),
            pending_reference_id: None,
            boundary_pending: false,
            checkpoints: Vec::new(),
            responses_replay: Default::default(),
            trigger: None,
            turn_completed: true,
            allowed_directories: Default::default(),
            allowed_instructions: Default::default(),
            loaded_skills: Vec::new(),
            host_inputs: Vec::new(),
        };
        let trigger = AgentMessage {
            message_id: "history-only".into(),
            sender_run_id: caller.to_string(),
            recipient_run_id: child.to_string(),
            kind: AgentMessageKind::Send,
            content: "must not become model input".into(),
            reply_to: None,
        };
        {
            let mut intents = runtime.shared.spawn_intents.lock().unwrap();
            intents.remove(&child);
            let mut admissions = runtime.shared.admissions.lock().unwrap();
            let execution =
                AgentRuntime::prepare_restored_execution(child, Some(caller), &intents).unwrap();
            AgentRuntime::install_restored_execution(
                child,
                Some(caller),
                &mut intents,
                &mut admissions,
                &execution,
            );
            runtime
                .register_restored(
                    &mut lock_runs(&runtime.shared.runs),
                    &intents,
                    execution,
                    (child, Some(caller), Role::Reviewer, config, caller),
                    restored,
                    &trigger,
                    None,
                )
                .unwrap();
        }
        let mut phase = runtime.entry(child).unwrap().phase_rx.clone();
        loop {
            let current = *phase.borrow_and_update();
            if current == AgentRunPhase::Waiting {
                break;
            }
            assert!(matches!(
                current,
                AgentRunPhase::Pending | AgentRunPhase::Running
            ));
            phase.changed().await.unwrap();
        }
        Self {
            runtime,
            parent,
            child,
            cancel,
            _storage: storage,
            _directory: directory,
        }
    }

    async fn cleanup(&self) {
        for run in [self.child, self.parent.caller_run_id()] {
            self.runtime.cancel(run).unwrap();
            self.runtime.wait(run).await.unwrap();
        }
    }
}

fn value(dispatch: crate::meta::DispatchResult) -> Value {
    assert!(!dispatch.result.is_error, "{:?}", dispatch.result.content);
    serde_json::from_str(&dispatch.result.content).unwrap()
}

#[tokio::test]
async fn history_only_restore_reports_saved_turn_once_from_wait_or_run_output() {
    for first_tool in ["wait", "run_output"] {
        let mut fixture = Fixture::restore(None).await;
        let input = json!({"run_id": fixture.child.to_string()});
        let dispatch = crate::meta::dispatch(&mut fixture.parent, first_tool, input.clone()).await;
        let first = value(dispatch);
        let turn = if first_tool == "wait" {
            assert_eq!(
                first["completed_run_ids"],
                json!([fixture.child.to_string()])
            );
            &first["runs"][0]
        } else {
            &first
        };
        assert_eq!(turn["phase"], "Waiting");
        assert_eq!(turn["status"], "turn_completed");
        assert_eq!(turn["output"], OUTPUT);
        assert!(turn["turn_generation"].as_u64().is_some());
        let output =
            value(crate::meta::dispatch(&mut fixture.parent, "run_output", input.clone()).await);
        assert_eq!(output["output"], OUTPUT);
        assert_eq!(output["turn_generation"], turn["turn_generation"]);
        {
            let wait = crate::meta::dispatch(&mut fixture.parent, "wait", input);
            tokio::pin!(wait);
            assert!(
                poll!(&mut wait).is_pending(),
                "acknowledged restored turns must not wake a second wait"
            );
            fixture.cancel.send_replace(RunInterrupt::Cancel);
            assert!(wait.await.result.is_error);
        }
        fixture.cleanup().await;
    }
}

#[tokio::test]
async fn history_only_restore_keeps_pending_questions_distinct_from_completed_turns() {
    for blocking in [false, true] {
        let mut fixture = Fixture::restore(Some(blocking)).await;
        let input = json!({"run_id": fixture.child.to_string()});
        let output =
            value(crate::meta::dispatch(&mut fixture.parent, "run_output", input.clone()).await);
        assert_eq!(output["phase"], "Waiting");
        assert_eq!(output["status"], "waiting");
        assert_eq!(output["output"], Value::Null);
        assert!(output.get("turn_generation").is_none());
        let wait = value(crate::meta::dispatch(&mut fixture.parent, "wait", input).await);
        assert_eq!(wait["completed_run_ids"], json!([]));
        assert_eq!(
            wait["attention_run_ids"],
            json!([fixture.child.to_string()])
        );
        assert_eq!(wait["runs"][0]["has_pending_question"], true);
        assert_eq!(wait["runs"][0]["needs_user_input"], blocking);
        assert_eq!(wait["runs"][0]["status"], "waiting");
        assert_eq!(wait["runs"][0]["output"], Value::Null);
        fixture.cleanup().await;
    }
}
