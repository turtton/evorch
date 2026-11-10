//! Actual tool failures must leave the Worker able to make another request.
use super::*;
use runtime::{AgentInvocationContext, AgentModel, RunStore, RuntimeError};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct PreparationModel {
    ask: bool,
    calls: AtomicUsize,
    continued: Notify,
    release: Notify,
    error_history: Mutex<Vec<Message>>,
}
#[async_trait::async_trait]
impl AgentModel for PreparationModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "handoff-fault".into()
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        if role != Role::Worker {
            return Ok(text_response("target", FinishReason::Stop));
        }
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.ask && call == 0 {
            return Ok(tool_response(
                "question",
                "ask_user",
                json!({"title":"Keep this question ID","blocking":false}),
            ));
        }
        if call == usize::from(self.ask) {
            return Ok(escalation_response());
        }
        *self.error_history.lock().unwrap() = messages.to_vec();
        self.continued.notify_one();
        self.release.notified().await;
        Ok(text_response(
            "Worker continued after a tool error",
            FinishReason::Stop,
        ))
    }
}
async fn preparation_failure(kind: &str) {
    let directory = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: directory.path().join("fault.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let model = Arc::new(PreparationModel {
        ask: kind == "question",
        calls: AtomicUsize::new(0),
        continued: Notify::new(),
        release: Notify::new(),
        error_history: Mutex::new(vec![]),
    });
    let bus = Arc::new(EventBus::new(256));
    let mut receiver = bus.subscribe();
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let owner = if kind == "ownership" {
        let path = directory.path().join("owners.db");
        let mut registry = runtime::ownership::Registry::open(&path).unwrap();
        let lease = runtime::ownership::Lease {
            owner_id: "fault-host".into(),
            generation: 1,
            expires_at: u64::MAX,
        };
        registry
            .start(&runtime::ownership::ThreadOwner::new(
                "source".into(),
                lease.clone(),
            ))
            .unwrap();
        rusqlite::Connection::open(&path).unwrap().execute_batch("CREATE TRIGGER fail_child BEFORE INSERT ON thread_owners WHEN NEW.thread_id!='source' BEGIN SELECT RAISE(ABORT,'ownership preparation fault'); END;").unwrap();
        Some(runtime::ownership::OwnerPermit {
            registry_path: path,
            thread_id: "source".into(),
            lease,
            run_id: None,
        })
    } else {
        None
    };
    if kind != "ownership" {
        let sql = if kind == "seed" {
            "CREATE TRIGGER fail_seed BEFORE INSERT ON run_contexts WHEN NEW.role='Orchestrator' BEGIN SELECT RAISE(ABORT,'seed preparation fault'); END;"
        } else {
            "CREATE TRIGGER fail_link BEFORE INSERT ON user_question_links BEGIN SELECT RAISE(ABORT,'question preparation fault'); END;"
        };
        rusqlite::Connection::open(&config.db_path)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }
    let source = runtime
        .delegate_chat(
            "source",
            Role::Worker,
            "original request".into(),
            RunConfig {
                ownership: owner,
                ..Default::default()
            },
        )
        .unwrap();
    model.continued.notified().await;
    assert_eq!(
        runtime.inspect_agent(source).unwrap().phase,
        AgentRunPhase::Running
    );
    let history = serde_json::to_string(&*model.error_history.lock().unwrap()).unwrap();
    assert!(history.contains("handoff preparation failed"), "{history}");
    assert!(
        history.contains(&format!("{kind} preparation fault")),
        "{history}"
    );
    let receipt = storage
        .handle()
        .handoff_for_source(source.to_string())
        .unwrap()
        .unwrap();
    assert!(!receipt.state.is_committed());
    assert!(runtime.recover_unstarted_lanes(None).unwrap().is_empty());
    assert!(!receiver.drain_pending_snapshot().iter().any(|e|matches!(&e.kind,EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {run_id,to:AgentRunPhase::Done|AgentRunPhase::Error|AgentRunPhase::Stopped,..}) if run_id==&source.to_string())));
    model.release.notify_one();
    assert_eq!(runtime.wait(source).await.unwrap(), AgentRunPhase::Done);
    assert_eq!(
        runtime.run_result(source).unwrap().as_deref(),
        Some("Worker continued after a tool error")
    );
}
#[tokio::test]
async fn seed_failure_is_a_tool_error_not_a_worker_terminal() {
    preparation_failure("seed").await;
}
#[tokio::test]
async fn question_failure_is_a_tool_error_not_a_worker_terminal() {
    preparation_failure("question").await;
}
#[tokio::test]
async fn ownership_failure_is_a_tool_error_not_a_worker_terminal() {
    preparation_failure("ownership").await;
}
