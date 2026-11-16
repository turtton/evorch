use super::*;
use runtime::learning_jobs::{LearningJobState, LearningOriginKind, current_origin};

struct PendingLesson(tokio::sync::mpsc::UnboundedSender<runtime::RunId>);
#[async_trait::async_trait]
impl runtime::AgentModel for PendingLesson {
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "offline".into()
    }
    async fn complete(
        &self,
        invocation: &runtime::AgentInvocationContext,
        role: runtime::Role,
        messages: &[providers::Message],
        tools: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        if invocation.category.as_deref() == Some("lesson") {
            self.0.send(invocation.run_id.parse().unwrap()).unwrap();
            return std::future::pending().await;
        }
        runtime::AgentModel::complete(&Model::default(), invocation, role, messages, tools).await
    }
}

#[tokio::test(start_paused = true)]
async fn cohort_timeout_reopens_chat_but_retains_cleanup_and_requeues_same_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("learning.db"),
        ..Default::default()
    };
    let store = storage::Storage::open(config.clone()).unwrap();
    let bus = Arc::new(event_bus::EventBus::new(4096));
    let executor = Arc::new(tools::ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    ));
    let (starts, mut started) = tokio::sync::mpsc::unbounded_channel();
    let runtime = AgentRuntime::new(bus, executor, Arc::new(PendingLesson(starts)))
        .with_learning_origin(current_origin(
            LearningOriginKind::Core,
            Some("fixture".into()),
        ))
        .with_run_store(runtime::RunStore::open(&config, store.handle()).unwrap())
        .with_learning(runtime::memory_queue::LearningSettings {
            writer: store.handle(),
            storage: config.clone(),
            project: "offline".into(),
            quick: runtime::ModelPreference {
                profile: "offline".into(),
                model: None,
                reasoning_effort: None,
            },
        });
    runtime.delegate_background(
        runtime::Role::Worker,
        "source evidence".into(),
        Default::default(),
    );
    let lesson = started.recv().await.unwrap();
    let (terminal, mut tails) = tokio::sync::mpsc::unbounded_channel();
    runtime
        .set_terminal_registered_probe(move |run, lease| {
            if run == lesson {
                assert!(terminal.send(lease).is_ok());
            }
        })
        .unwrap();
    let read = storage::Database::open(&config).unwrap();
    let job = read
        .learning_jobs(&[LearningJobState::Started])
        .unwrap()
        .pop()
        .unwrap();
    let evidence = read.learning_job_evidence(&job.task_id).unwrap().unwrap();
    // An actual terminal lease delays the pipeline return without blocking a
    // Tokio worker or relying on SQLite busy_timeout / wall-clock scheduling.
    let gate = HostRestartGate::default();
    let core = RestartCoordinator::from_parts(
        runtime.clone(),
        gate.clone(),
        vec![runtime.producer_gate()],
        Default::default(),
    );
    let id = core.request().unwrap();
    let mut changes = core.subscribe();
    let suspension = loop {
        if let Some(suspension) = core.0.state.lock().unwrap().suspension.clone() {
            break suspension;
        }
        changes.changed().await.unwrap();
    };
    assert_eq!(gate.phase(), HostRestartPhase::Open);
    assert_eq!(core.request().unwrap(), id);
    let tail = tails.recv().await.unwrap();
    tokio::time::advance(super::super::preparing::LEARNING_COHORT_LIMIT).await;
    phase(&core, RestartPhase::Running).await;
    assert!(runtime.learning_gate().is_open());
    assert_eq!(
        runtime.quiesce_state(),
        runtime::quiesce::QuiesceState::Running
    );
    assert!(core.0.state.lock().unwrap().attempt.is_none());
    assert!(
        started.try_recv().is_err(),
        "owned terminal tail must not invent a second attempt"
    );
    drop(tail);
    suspension.wait().await.unwrap();
    started.recv().await.unwrap();
    let retried = read.learning_job(&job.task_id).unwrap().unwrap();
    assert_eq!(retried.attempt, job.attempt + 1);
    assert_eq!(retried.task_id, job.task_id);
    assert_eq!(
        read.learning_job_evidence(&job.task_id).unwrap().unwrap(),
        evidence
    );
    let pause = runtime.pause_learning();
    runtime
        .suspend_learning(&pause)
        .unwrap()
        .wait()
        .await
        .unwrap();
}
