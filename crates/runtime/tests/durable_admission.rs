use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use event_bus::{EventBus, EventKind, EventReceiver, OrchestratorEvent, RunPurpose};
use providers::{ChatResponse, FinishReason, Message, ToolSpec, Usage};
use runtime::orchestration::{
    delivery::FixtureDeliveryAdapter,
    ledger::{GoalLedger, OrchestrationSettings},
    supervisor::{GoalSupervisor, SupervisorHandle},
};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RunId, RuntimeError};
use storage::entity::{TaskContinuation, TaskStatus};
use tokio::sync::Notify;

#[derive(Default)]
struct AdmissionModel {
    entered: Notify,
    release: Notify,
    admissions: AtomicUsize,
    completions: AtomicUsize,
    fail_first: bool,
}

#[async_trait]
impl AgentModel for AdmissionModel {
    fn requires_admission(&self) -> bool {
        true
    }

    async fn admit(&self, _: &AgentInvocationContext, _: Role) -> Result<(), RuntimeError> {
        let attempt = self.admissions.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.notified().await;
        if self.fail_first && attempt == 0 {
            Err(RuntimeError::Model {
                reason: "catalog offline".into(),
            })
        } else {
            Ok(())
        }
    }

    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.completions.fetch_add(1, Ordering::SeqCst);
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: vec![],
            },
            usage: Usage::default(),
            finish_reason: FinishReason::Stop,
        })
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "controlled".into()
    }
}

struct Fixture {
    bus: Arc<EventBus>,
    model: Arc<AdmissionModel>,
    runtime: AgentRuntime,
    handle: SupervisorHandle,
    events: EventReceiver,
}

impl Fixture {
    async fn new(fail_first: bool) -> Self {
        let bus = Arc::new(EventBus::new(256));
        let model = Arc::new(AdmissionModel {
            fail_first,
            ..Default::default()
        });
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::with_standard_tools(
                bus.clone(),
                Arc::new(sandbox::DirectSandbox::new_unchecked()),
            )),
            model.clone(),
        );
        let handle = GoalSupervisor::spawn(
            runtime.clone(),
            bus.clone(),
            Arc::new(FixtureDeliveryAdapter::default()),
            OrchestrationSettings {
                max_continuations: 4,
                stall_after_secs: 86_400,
                ..Default::default()
            },
        );
        let events = bus.subscribe();
        let mut ledger = GoalLedger::new(&OrchestratorEvent::GoalCreated {
            goal_id: "goal".into(),
            session_id: "session".into(),
            project_id: "project".into(),
            thread_id: "thread".into(),
            goal: "resume task".into(),
            references: vec![],
            constraints: vec![],
            repo: "repo".into(),
            base_ref: "main".into(),
            root_run_id: "run-100".into(),
        });
        ledger
            .apply(&OrchestratorEvent::RunAttached {
                goal_id: "goal".into(),
                run_id: "run-101".into(),
                parent_run_id: Some("run-100".into()),
                role: "worker".into(),
                purpose: RunPurpose::Implement,
            })
            .unwrap();
        ledger.apply(&OrchestratorEvent::TaskProgressed { task_id: "task".into(), run_id: "run-101".into(), progress: serde_json::json!({"status":"failed", "attempts":0, "failure_reason":"interrupted"}), reason: "interrupted".into() }).unwrap();
        handle
            .adopt(vec![(ledger.snapshot().clone(), vec![])])
            .unwrap();
        let mut fixture = Self {
            bus,
            model,
            runtime,
            handle,
            events,
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    fixture.events.recv().await.unwrap().kind,
                    EventKind::Orchestrator(OrchestratorEvent::GoalStateChanged { .. })
                ) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        fixture
    }

    fn task(&self) -> TaskContinuation {
        serde_json::from_value(self.handle.snapshot("goal").unwrap().task_progress["task"].clone())
            .unwrap()
    }

    fn run(&self) -> RunId {
        RunId::new(
            self.handle.snapshot("goal").unwrap().task_runs["task"]
                .strip_prefix("run-")
                .unwrap()
                .parse()
                .unwrap(),
        )
    }

    async fn status(&mut self, expected: TaskStatus) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if self.task().status == expected {
                    break;
                }
                self.events.recv().await.unwrap();
            }
        })
        .await
        .unwrap_or_else(|_| panic!("expected {expected:?}, got {:?}", self.task()));
    }

    async fn retry(&self) -> RunId {
        self.handle.retry_task("task").unwrap();
        tokio::time::timeout(Duration::from_secs(2), self.model.entered.notified())
            .await
            .unwrap();
        self.run()
    }
}

#[tokio::test]
async fn cancelled_retry_during_pending_admission_never_registers_or_starts_worker() {
    // Given: a retry whose provider admission is pending.
    let mut fixture = Fixture::new(false).await;
    let run = fixture.retry().await;
    // When: cancellation is processed before admission succeeds.
    fixture.handle.cancel_task("task").unwrap();
    fixture.status(TaskStatus::Cancelled).await;
    fixture.model.release.notify_one();
    let _ = tokio::time::timeout(Duration::from_secs(2), fixture.runtime.wait(run))
        .await
        .unwrap();
    // Then: cancellation wins without registering or executing the worker.
    assert_eq!(fixture.task().status, TaskStatus::Cancelled);
    assert!(
        fixture.runtime.list_agents().is_empty(),
        "cancelled admission registered a worker"
    );
    assert_eq!(fixture.model.completions.load(Ordering::SeqCst), 0);
    fixture.bus.emit(event_bus::Event::new(
        event_bus::MessageEvent::MessageDelta {
            delta: String::new(),
            run_id: None,
        },
    ));
    loop {
        let event = fixture.events.recv().await.unwrap();
        if matches!(event.kind, EventKind::Message(_)) {
            break;
        }
        assert!(
            !matches!(event.kind, EventKind::Lifecycle(_) | EventKind::Tool(_)),
            "unexpected side effect: {event:?}"
        );
    }
}

#[tokio::test]
async fn failed_retry_admission_marks_generation_failed_and_allows_next_retry() {
    // Given: the first retry's admission fails, the next admission succeeds.
    let mut fixture = Fixture::new(true).await;
    let failed_run = fixture.retry().await;
    fixture.model.release.notify_one();
    assert!(matches!(
        fixture.runtime.wait(failed_run).await,
        Err(RuntimeError::Model { .. })
    ));
    // When: the supervisor receives the failed generation's admission result.
    fixture.status(TaskStatus::Failed).await;
    // Then: it persists the reason on that generation and permits a new retry.
    assert_eq!(fixture.run(), failed_run);
    assert_eq!(fixture.task().attempts, 1);
    assert!(
        fixture
            .task()
            .failure_reason
            .unwrap()
            .starts_with("ProviderUnavailable:")
    );
    assert!(fixture.runtime.list_agents().is_empty());
    let next_run = fixture.retry().await;
    assert_ne!(next_run, failed_run);
    assert_eq!(fixture.task().attempts, 2);
    fixture.model.release.notify_one();
    assert_eq!(
        fixture.runtime.wait(next_run).await.unwrap(),
        event_bus::AgentRunPhase::Done
    );
    fixture.status(TaskStatus::Completed).await;
    assert_eq!(fixture.model.completions.load(Ordering::SeqCst), 1);
}
