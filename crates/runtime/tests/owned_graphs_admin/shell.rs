use std::sync::Arc;

struct Direct;
impl runtime::SandboxFactory for Direct {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

pub fn factory(case: &str, bus: Arc<event_bus::EventBus>) -> super::fixture::RuntimeSetup {
    let (executor, model, _) = super::fixture::default_factory(case, bus.clone());
    if case != "release-shell-teardown" {
        return (executor, model, None);
    }
    (
        Arc::new(tools::ToolExecutor::with_standard_tools(
            bus,
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        )),
        model,
        Some(Arc::new(Direct)),
    )
}

pub fn request() -> providers::ChatResponse {
    // A real admitted shell job whose spawn fails after preflight. Its opaque
    // identity remains terminal but teardown-unconfirmed in the actual executor.
    providers::ChatResponse {
        message: providers::Message {
            role: providers::Role::Assistant,
            content: vec![providers::ContentBlock::ToolUse {
                id: "unconfirmed".into(),
                name: "shell".into(),
                input: serde_json::json!({"command":"invalid\0command"}),
            }],
        },
        usage: Default::default(),
        usage_reported: true,
        finish_reason: providers::FinishReason::ToolUse,
    }
}
