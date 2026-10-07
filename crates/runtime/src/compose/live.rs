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

    pub fn replace(&self, model: Arc<dyn AgentModel>) {
        let previous = {
            let mut target = self
                .0
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    fn invocation_snapshot(&self) -> Option<Arc<dyn AgentModel>> {
        let current = self.current();
        Some(current.invocation_snapshot().unwrap_or(current))
    }

    fn benchmark_settings(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        tools: &[ToolSpec],
    ) -> Result<crate::benchmark::BenchmarkModelSettings, RuntimeError> {
        self.current().benchmark_settings(invocation, role, tools)
    }

    fn freeze_for_benchmark(
        self: Arc<Self>,
        settings: crate::benchmark::BenchmarkModelSettings,
    ) -> Result<Arc<dyn AgentModel>, RuntimeError> {
        self.current().freeze_for_benchmark(settings)
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
        tools: &[ToolSpec],
    ) -> Result<Option<providers::CompactionResult>, RuntimeError> {
        self.current()
            .compact_context(invocation, role, messages, tools)
            .await
    }

    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.current()
            .complete(invocation, role, messages, tools)
            .await
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
        self.current()
            .complete_streaming(invocation, role, messages, tools, bus)
            .await
    }

    fn selected_model(&self, role: Role, category: Option<&str>) -> String {
        self.current().selected_model(role, category)
    }

    fn selected_reasoning_effort(&self, role: Role, category: Option<&str>) -> Option<String> {
        self.current().selected_reasoning_effort(role, category)
    }

    fn catalog_context_window(&self, selected_model: &str) -> Option<u64> {
        self.current().catalog_context_window(selected_model)
    }

    fn available_profiles(&self) -> Vec<super::ProfileSummary> {
        SwitchableModel::available_profiles(self)
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
