use super::*;
use std::sync::atomic::AtomicUsize;

struct CountingModel(AtomicUsize);
#[async_trait::async_trait]
impl AgentModel for CountingModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "counted".into()
    }
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(providers::ChatResponse {
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text {
                    text: "done".into(),
                }],
            },
            usage: Default::default(),
            usage_reported: true,
            finish_reason: providers::FinishReason::Stop,
        })
    }
}
fn fixture() -> (AgentRuntime, Arc<CountingModel>) {
    let bus = Arc::new(EventBus::new(64));
    let model = Arc::new(CountingModel(AtomicUsize::new(0)));
    (
        AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone()),
        model,
    )
}
fn candidate(runtime: &AgentRuntime) -> PreparedRunCandidate {
    let run = runtime.reserve_run_id();
    let execution = ExecutionControl::new(false);
    let mut intents = runtime.shared.spawn_intents.lock().unwrap();
    intents.insert(
        run,
        cancellation::SpawnIntent {
            execution: Some(execution.clone()),
            ..Default::default()
        },
    );
    runtime
        .build_run_candidate(
            run,
            None,
            Role::Worker,
            "work".into(),
            RunConfig::default(),
            RunContinuation::Fresh,
            None,
            execution,
            &intents,
            true,
        )
        .unwrap()
}

#[tokio::test]
async fn all_candidates_can_be_prepared_without_publishing_or_starting_any_member() {
    let (runtime, model) = fixture();
    let first = candidate(&runtime);
    let second = candidate(&runtime);
    assert!(runtime.list_agents().is_empty());
    assert!(runtime.shared.review_runs.lock().unwrap().is_empty());
    assert!(runtime.shared.learning_runs.lock().unwrap().is_empty());
    assert_eq!(model.0.load(Ordering::SeqCst), 0);
    let (gate, closed) = watch::channel(false);
    let a = runtime.publish_run_candidate(first, Some(closed.clone()));
    let b = runtime.publish_run_candidate(second, Some(closed));
    assert_eq!(runtime.list_agents().len(), 2);
    assert_eq!(model.0.load(Ordering::SeqCst), 0);
    assert!(runtime.shared.learning_runs.lock().unwrap().is_empty());
    gate.send_replace(true);
    runtime.wait(a).await.unwrap();
    runtime.wait(b).await.unwrap();
    assert_eq!(model.0.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn dropping_an_unopened_gate_never_calls_the_provider() {
    let (runtime, model) = fixture();
    let prepared = candidate(&runtime);
    let (gate, closed) = watch::channel(false);
    let run = runtime.publish_run_candidate(prepared, Some(closed));
    let join = lock_runs(&runtime.shared.runs)
        .get_mut(&run)
        .unwrap()
        ._join
        .take()
        .unwrap();
    drop(gate);
    join.await.unwrap();
    assert_eq!(model.0.load(Ordering::SeqCst), 0);
}
