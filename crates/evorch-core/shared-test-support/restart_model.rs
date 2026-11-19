//! Shared actual-input checkpoint model and direct sandbox for process fixtures.
use runtime::RuntimeError;
use std::sync::{Arc, Mutex};
#[derive(Default)]
pub struct Model(
    pub std::sync::atomic::AtomicUsize,
    pub Mutex<std::collections::BTreeMap<String, runtime::quiesce::ProviderInput>>,
);
#[async_trait::async_trait]
impl runtime::AgentModel for Model {
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "restart-fixture".into()
    }
    fn requires_admission(&self) -> bool {
        false
    }
    fn checkpoint_provider_input(&self, run: &str) -> Option<runtime::quiesce::ProviderInput> {
        self.1.lock().unwrap().get(run).cloned()
    }
    async fn admit(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn complete(
        &self,
        invocation: &runtime::AgentInvocationContext,
        _: runtime::Role,
        messages: &[providers::Message],
        tools: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Live idle fixtures checkpoint the actual input seen by this model,
        // including run affinity; never fabricate a fresh request at capture.
        self.1.lock().unwrap().insert(
            invocation.run_id.clone(),
            runtime::quiesce::ProviderInput {
                profile: "restart-fixture".into(),
                protocol: model::ApiProtocol::OpenAiCompletions,
                edit_tool: config::EditToolConfig::Edit,
                affinity_key: invocation.run_id.clone(),
                replay: None,
                request: providers::ChatRequest {
                    model: "restart-fixture".into(),
                    messages: messages.to_vec(),
                    tools: tools.to_vec(),
                    tool_choice: None,
                    temperature: None,
                    max_tokens: None,
                    reasoning_effort: None,
                    service_tier: None,
                    output_schema: None,
                    observation: None,
                    responses_replay: None,
                },
            },
        );
        Ok(providers::ChatResponse {
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text {
                    text: "completed".into(),
                }],
            },
            usage: Default::default(),
            usage_reported: true,
            finish_reason: providers::FinishReason::Stop,
        })
    }
}
pub struct Direct;
impl runtime::SandboxFactory for Direct {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}
