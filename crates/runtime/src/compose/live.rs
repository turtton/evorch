use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use providers::{ChatResponse, Message, ToolSpec};

use crate::{AgentInvocationContext, AgentModel, Role, RuntimeError};

#[derive(Clone)]
pub struct SwitchableModel(Arc<RwLock<Arc<dyn AgentModel>>>);

impl SwitchableModel {
    pub fn available_profiles(&self) -> Vec<super::ProfileSummary> {
        self.current().available_profiles()
    }

    pub fn new(model: Arc<dyn AgentModel>) -> Self {
        Self(Arc::new(RwLock::new(model)))
    }

    /// Swaps in a reconfigured model. Chain pins (ADR 0030) and the editing interfaces
    /// used by previous requests carry over; cooldowns do not.
    pub fn replace(&self, model: Arc<dyn AgentModel>) {
        let previous = {
            let mut target = self
                .0
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (run_id, pins) in target.route_pins_by_run() {
                model.restore_route_pins(&run_id, &pins);
            }
            model.restore_edit_tools(&target.edit_tools_by_run());
            std::mem::replace(&mut *target, model)
        };
        drop(previous);
    }

    fn current(&self) -> Arc<dyn AgentModel> {
        Arc::clone(
            &self
                .0
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

#[async_trait]
impl AgentModel for SwitchableModel {
    fn narrows_edit_tools(&self) -> bool {
        true
    }

    fn edit_tool_for_run(
        &self,
        run_id: &str,
        role: Role,
        category: Option<&str>,
    ) -> Option<config::EditToolConfig> {
        self.current().edit_tool_for_run(run_id, role, category)
    }

    fn edit_tools_by_run(
        &self,
    ) -> std::collections::BTreeMap<(String, String), config::EditToolConfig> {
        self.current().edit_tools_by_run()
    }

    fn restore_edit_tools(
        &self,
        history: &std::collections::BTreeMap<(String, String), config::EditToolConfig>,
    ) {
        self.current().restore_edit_tools(history);
    }

    fn model_switch_prepared(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        selected: &str,
    ) {
        self.current()
            .model_switch_prepared(invocation, role, selected);
    }

    fn invocation_snapshot(&self) -> Option<Arc<dyn AgentModel>> {
        let current = self.current();
        let current = current.invocation_snapshot().unwrap_or(current);
        if current.narrows_edit_tools() {
            Some(current)
        } else {
            Some(Arc::new(Self::new(current)))
        }
    }

    fn web_search_provider(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        tools: &[ToolSpec],
        usage_sink: Arc<dyn Fn(providers::Usage) + Send + Sync>,
    ) -> Result<Option<Arc<dyn tools::search::SearchProvider>>, RuntimeError> {
        let current = self.current();
        let tools = crate::edit_tools::for_model(current.as_ref(), tools);
        current.web_search_provider(invocation, role, &tools, usage_sink)
    }

    fn image_generation_enabled(&self) -> bool {
        self.current().image_generation_enabled()
    }

    async fn generate_image(
        &self,
        invocation: &AgentInvocationContext,
        request: providers::ImageGenerationRequest,
    ) -> Result<crate::image_generation::GeneratedImage, String> {
        self.current().generate_image(invocation, request).await
    }

    async fn benchmark_settings(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        tools: &[ToolSpec],
    ) -> Result<crate::benchmark::BenchmarkModelSettings, RuntimeError> {
        self.current()
            .benchmark_settings(invocation, role, tools)
            .await
    }

    fn freeze_for_benchmark(
        self: Arc<Self>,
        settings: crate::benchmark::BenchmarkModelSettings,
    ) -> Result<Arc<dyn AgentModel>, RuntimeError> {
        let frozen = self.current().freeze_for_benchmark(settings)?;
        if frozen.narrows_edit_tools() {
            Ok(frozen)
        } else {
            Ok(Arc::new(Self::new(frozen)))
        }
    }
    fn requires_admission(&self) -> bool {
        self.current().requires_admission()
    }

    async fn admit(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
    ) -> Result<(), RuntimeError> {
        self.current().admit(invocation, role).await
    }

    async fn compact_context(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        instructions: &[Message],
        tools: &[ToolSpec],
    ) -> Result<Option<providers::CompactionResult>, RuntimeError> {
        let current = self.current();
        let tools = crate::edit_tools::for_model(current.as_ref(), tools);
        current
            .compact_context(invocation, role, messages, instructions, &tools)
            .await
    }

    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let current = self.current();
        let tools = crate::edit_tools::for_model(current.as_ref(), tools);
        current.complete(invocation, role, messages, &tools).await
    }

    async fn complete_structured(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        schema: &providers::JsonSchema,
    ) -> Result<ChatResponse, RuntimeError> {
        self.current()
            .complete_structured(invocation, role, messages, schema)
            .await
    }

    async fn complete_streaming(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
        bus: &event_bus::EventBus,
    ) -> Result<ChatResponse, RuntimeError> {
        let current = self.current();
        let tools = crate::edit_tools::for_model(current.as_ref(), tools);
        current
            .complete_streaming(invocation, role, messages, &tools, bus)
            .await
    }

    async fn prepare_replay_context(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
        replay: &crate::ResponsesReplayState,
    ) -> Result<(), RuntimeError> {
        let current = self.current();
        let tools = crate::edit_tools::for_model(current.as_ref(), tools);
        current
            .prepare_replay_context(invocation, role, messages, &tools, replay)
            .await
    }

    async fn resolve_replay_scope(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        tools: &[ToolSpec],
    ) -> Result<Option<providers::ResolvedResponsesScope>, RuntimeError> {
        let current = self.current();
        let tools = crate::edit_tools::for_model(current.as_ref(), tools);
        current.resolve_replay_scope(invocation, role, &tools).await
    }

    async fn complete_streaming_with_replay(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
        bus: &event_bus::EventBus,
        replay: &crate::ResponsesReplayState,
    ) -> Result<ChatResponse, RuntimeError> {
        let current = self.current();
        let tools = crate::edit_tools::for_model(current.as_ref(), tools);
        current
            .complete_streaming_with_replay(invocation, role, messages, &tools, bus, replay)
            .await
    }

    fn selected_model(&self, role: Role, category: Option<&str>) -> String {
        self.current().selected_model(role, category)
    }

    fn selected_model_for_run(&self, run_id: &str, role: Role, category: Option<&str>) -> String {
        self.current()
            .selected_model_for_run(run_id, role, category)
    }

    fn selected_reasoning_effort(&self, role: Role, category: Option<&str>) -> Option<String> {
        self.current().selected_reasoning_effort(role, category)
    }

    fn binding_preview(
        &self,
        role: Role,
        category: Option<&str>,
    ) -> Option<config::types::agents::ResolvedAgentBinding> {
        self.current().binding_preview(role, category)
    }

    fn catalog_context_window(&self, selected_model: &str) -> Option<u64> {
        self.current().catalog_context_window(selected_model)
    }

    fn available_profiles(&self) -> Vec<super::ProfileSummary> {
        SwitchableModel::available_profiles(self)
    }

    fn route_pins(&self, run_id: &str) -> Vec<crate::RoutePinRecord> {
        self.current().route_pins(run_id)
    }

    fn restore_route_pins(&self, run_id: &str, pins: &[crate::RoutePinRecord]) {
        self.current().restore_route_pins(run_id, pins);
    }

    fn route_pins_by_run(&self) -> std::collections::BTreeMap<String, Vec<crate::RoutePinRecord>> {
        self.current().route_pins_by_run()
    }
}

pub struct UnconfiguredModel;

#[async_trait]
impl AgentModel for UnconfiguredModel {
    fn requires_admission(&self) -> bool {
        true
    }

    async fn admit(
        &self,
        _invocation: &AgentInvocationContext,
        _role: Role,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::Model {
            reason: "no provider configured — open Settings".into(),
        })
    }

    async fn complete(
        &self,
        _invocation: &AgentInvocationContext,
        _role: Role,
        _messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        Err(RuntimeError::Model {
            reason: "no provider configured — open Settings".into(),
        })
    }

    fn selected_model(&self, role: Role, _category: Option<&str>) -> String {
        format!("unresolved:{}", super::role_key(role))
    }
}
