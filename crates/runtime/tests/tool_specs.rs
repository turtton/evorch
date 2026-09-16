mod support;

use std::sync::Arc;

use event_bus::{AgentRunPhase, EventBus};
use providers::{ChatResponse, FinishReason, Message, ToolSpec};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RuntimeError};
use tools::{Edit, Read, Tool, ToolExecutor};

#[derive(Default)]
struct ObservingModel(tokio::sync::Mutex<Vec<ToolSpec>>);

#[async_trait::async_trait]
impl AgentModel for ObservingModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }

    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        *self.0.lock().await = tools.to_vec();
        Ok(support::text_response("done", FinishReason::Stop))
    }
}

#[tokio::test]
async fn model_receives_only_registered_tools_with_real_schemas() {
    // Given: a worker runtime with read/edit but no other executor tools.
    let bus = Arc::new(EventBus::new(64));
    let mut executor = ToolExecutor::new(Arc::clone(&bus));
    executor.register(Arc::new(Read)).unwrap();
    executor.register(Arc::new(Edit)).unwrap();
    let model = Arc::new(ObservingModel::default());
    let runtime = AgentRuntime::new(bus, Arc::new(executor), model.clone());

    // When: a real agent loop requests its first model completion.
    let run = runtime.delegate_background(Role::Worker, "inspect".into(), RunConfig::default());
    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));

    // Then: schemas and descriptions survive the runtime path and visibility gate.
    let specs = model.0.lock().await;
    for tool in [&Read as &dyn Tool, &Edit as &dyn Tool] {
        let spec = specs.iter().find(|spec| spec.name == tool.name()).unwrap();
        assert_eq!(spec.input_schema, tool.schema());
        assert_eq!(spec.description, tool.description());
    }
    assert!(!specs.iter().any(|spec| spec.name == "shell"));
    assert!(!specs.iter().any(|spec| spec.name == "skill_load"));
    assert!(specs.iter().any(|spec| spec.name == "escalate"));
}
