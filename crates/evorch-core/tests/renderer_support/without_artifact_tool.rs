//! The model of a core that predates the artifact provider, shared by the
//! cache tests that resume a conversation from before it.
use async_trait::async_trait;
use std::sync::Arc;

/// The model of a core that predates the artifact provider: it forwards every
/// call of the production model and hides `inspect_artifact` from the tool array,
/// which is exactly the catalog such a core sent. The switch ends the pretence, as
/// the upgraded core does when it resumes the conversation.
pub struct WithoutArtifactTool(
    pub Arc<dyn runtime::AgentModel>,
    pub Arc<std::sync::atomic::AtomicBool>,
);

impl WithoutArtifactTool {
    fn visible(&self, tools: &[providers::ToolSpec]) -> Vec<providers::ToolSpec> {
        let hidden = self.1.load(std::sync::atomic::Ordering::SeqCst);
        tools
            .iter()
            .filter(|tool| !(hidden && tool.name.ends_with(runtime::artifacts::INSPECT_ARTIFACT)))
            .cloned()
            .collect()
    }
}

type Invocation = runtime::AgentInvocationContext;
type Rt<T> = Result<T, runtime::RuntimeError>;

#[async_trait]
impl runtime::AgentModel for WithoutArtifactTool {
    fn validate_checkpoint_compatibility(
        &self,
        invocation: &Invocation,
        role: runtime::Role,
        pins: &[runtime::RoutePinRecord],
        input: Option<&runtime::quiesce::ProviderInput>,
    ) -> Rt<()> {
        self.0
            .validate_checkpoint_compatibility(invocation, role, pins, input)
    }
    fn adopt_recovery_checkpoint(
        &self,
        run_id: &str,
        pins: &[runtime::RoutePinRecord],
        input: Option<&runtime::quiesce::ProviderInput>,
    ) {
        self.0.adopt_recovery_checkpoint(run_id, pins, input);
    }
    fn checkpoint_provider_input(&self, run_id: &str) -> Option<runtime::quiesce::ProviderInput> {
        self.0.checkpoint_provider_input(run_id)
    }
    fn restore_checkpoint_provider_state(
        &self,
        run_id: &str,
        role: runtime::Role,
        category: Option<&str>,
        input: &runtime::quiesce::ProviderInput,
    ) {
        self.0
            .restore_checkpoint_provider_state(run_id, role, category, input);
    }
    fn narrows_edit_tools(&self) -> bool {
        self.0.narrows_edit_tools()
    }
    fn edit_tool_for_run(
        &self,
        run_id: &str,
        role: runtime::Role,
        category: Option<&str>,
    ) -> Option<config::EditToolConfig> {
        self.0.edit_tool_for_run(run_id, role, category)
    }
    fn edit_tools_by_run(
        &self,
    ) -> std::collections::BTreeMap<(String, String), config::EditToolConfig> {
        self.0.edit_tools_by_run()
    }
    fn restore_edit_tools(
        &self,
        history: &std::collections::BTreeMap<(String, String), config::EditToolConfig>,
    ) {
        self.0.restore_edit_tools(history);
    }
    fn model_switch_prepared(&self, invocation: &Invocation, role: runtime::Role, selected: &str) {
        self.0.model_switch_prepared(invocation, role, selected);
    }
    fn image_generation_enabled(&self) -> bool {
        self.0.image_generation_enabled()
    }
    fn web_search_provider(
        &self,
        invocation: &Invocation,
        role: runtime::Role,
        tools: &[providers::ToolSpec],
        usage_sink: Arc<dyn Fn(providers::Usage) + Send + Sync>,
    ) -> Rt<Option<Arc<dyn tools::search::SearchProvider>>> {
        self.0
            .web_search_provider(invocation, role, &self.visible(tools), usage_sink)
    }
    fn requires_admission(&self) -> bool {
        self.0.requires_admission()
    }
    async fn admit(&self, invocation: &Invocation, role: runtime::Role) -> Rt<()> {
        self.0.admit(invocation, role).await
    }
    async fn compact_context(
        &self,
        invocation: &Invocation,
        role: runtime::Role,
        messages: &[providers::Message],
        instructions: &[providers::Message],
        tools: &[providers::ToolSpec],
    ) -> Rt<Option<providers::CompactionResult>> {
        self.0
            .compact_context(
                invocation,
                role,
                messages,
                instructions,
                &self.visible(tools),
            )
            .await
    }
    async fn prepare_replay_context(
        &self,
        invocation: &Invocation,
        role: runtime::Role,
        messages: &[providers::Message],
        tools: &[providers::ToolSpec],
        replay: &runtime::ResponsesReplayState,
    ) -> Rt<()> {
        self.0
            .prepare_replay_context(invocation, role, messages, &self.visible(tools), replay)
            .await
    }
    async fn resolve_replay_scope(
        &self,
        invocation: &Invocation,
        role: runtime::Role,
        tools: &[providers::ToolSpec],
    ) -> Rt<Option<providers::ResolvedResponsesScope>> {
        self.0
            .resolve_replay_scope(invocation, role, &self.visible(tools))
            .await
    }
    async fn complete_streaming_with_replay(
        &self,
        invocation: &Invocation,
        role: runtime::Role,
        messages: &[providers::Message],
        tools: &[providers::ToolSpec],
        bus: &event_bus::EventBus,
        replay: &runtime::ResponsesReplayState,
    ) -> Rt<providers::ChatResponse> {
        self.0
            .complete_streaming_with_replay(
                invocation,
                role,
                messages,
                &self.visible(tools),
                bus,
                replay,
            )
            .await
    }
    async fn complete(
        &self,
        invocation: &Invocation,
        role: runtime::Role,
        messages: &[providers::Message],
        tools: &[providers::ToolSpec],
    ) -> Rt<providers::ChatResponse> {
        self.0
            .complete(invocation, role, messages, &self.visible(tools))
            .await
    }
    async fn complete_structured(
        &self,
        invocation: &Invocation,
        role: runtime::Role,
        messages: &[providers::Message],
        schema: &providers::JsonSchema,
    ) -> Rt<providers::ChatResponse> {
        self.0
            .complete_structured(invocation, role, messages, schema)
            .await
    }
    async fn complete_streaming(
        &self,
        invocation: &Invocation,
        role: runtime::Role,
        messages: &[providers::Message],
        tools: &[providers::ToolSpec],
        bus: &event_bus::EventBus,
    ) -> Rt<providers::ChatResponse> {
        self.0
            .complete_streaming(invocation, role, messages, &self.visible(tools), bus)
            .await
    }
    fn selected_model(&self, role: runtime::Role, category: Option<&str>) -> String {
        self.0.selected_model(role, category)
    }
    fn chain_models(&self, role: runtime::Role, category: Option<&str>) -> Vec<String> {
        self.0.chain_models(role, category)
    }
    fn selected_model_for_run(
        &self,
        run_id: &str,
        role: runtime::Role,
        category: Option<&str>,
    ) -> String {
        self.0.selected_model_for_run(run_id, role, category)
    }
    fn catalog_context_window(&self, selected_model: &str) -> Option<u64> {
        self.0.catalog_context_window(selected_model)
    }
    fn available_profiles(&self) -> Vec<runtime::compose::ProfileSummary> {
        self.0.available_profiles()
    }
    fn selected_reasoning_effort(
        &self,
        role: runtime::Role,
        category: Option<&str>,
    ) -> Option<String> {
        self.0.selected_reasoning_effort(role, category)
    }
    fn binding_preview(
        &self,
        role: runtime::Role,
        category: Option<&str>,
    ) -> Option<config::types::agents::ResolvedAgentBinding> {
        self.0.binding_preview(role, category)
    }
    fn route_pins(&self, run_id: &str) -> Vec<runtime::RoutePinRecord> {
        self.0.route_pins(run_id)
    }
    fn restore_route_pins(&self, run_id: &str, pins: &[runtime::RoutePinRecord]) {
        self.0.restore_route_pins(run_id, pins);
    }
    fn output_limit_catalogs(&self) -> Option<runtime::compose::OutputLimitCatalogs> {
        self.0.output_limit_catalogs()
    }
    fn restore_output_limit_catalogs(&self, shared: &runtime::compose::OutputLimitCatalogs) {
        self.0.restore_output_limit_catalogs(shared);
    }
    fn route_pins_by_run(
        &self,
    ) -> std::collections::BTreeMap<String, Vec<runtime::RoutePinRecord>> {
        self.0.route_pins_by_run()
    }
}
