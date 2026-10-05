use super::*;
use crate::benchmark::{BenchmarkModelSettings, unsupported};

pub(super) fn freeze(
    model: Arc<RoutedModel>,
    settings: BenchmarkModelSettings,
) -> Result<Arc<dyn AgentModel>, RuntimeError> {
    if settings
        .preference
        .model
        .as_deref()
        .is_none_or(str::is_empty)
        || settings.generation.top_p.is_some()
    {
        return Err(unsupported(
            "explicit model required; top_p is not supported by provider requests",
        ));
    }
    let invocation = AgentInvocationContext {
        model_preference: Some(settings.preference.clone()),
        ..Default::default()
    };
    // Route validation must not fall back to the current role binding.
    let route = model
        .resolve_invocation(&invocation, Role::Worker, &[])?
        .route;
    if model.providers[&route.profile].profile.api_protocol != settings.protocol {
        return Err(unsupported(
            "candidate API protocol differs from checkpoint; cross-protocol generation parity is not supported",
        ));
    }
    match settings.protocol {
        model::ApiProtocol::OpenAiCodexResponses
            if settings.generation.temperature.is_some()
                || settings.generation.max_tokens.is_some() =>
        {
            return Err(unsupported(
                "Codex wire format does not preserve explicit temperature or max_tokens",
            ));
        }
        model::ApiProtocol::AnthropicMessages
            if settings.preference.reasoning_effort.is_some()
                || settings.service_tier.is_some() =>
        {
            return Err(unsupported(
                "Anthropic wire format does not preserve explicit reasoning_effort or service_tier",
            ));
        }
        model::ApiProtocol::OpenAiResponses => {
            return Err(unsupported(
                "OpenAI Responses generation parity is not implemented",
            ));
        }
        _ => {}
    }
    let (model_id, _) = config::types::provider::parse_model_speed(&route.model_id);
    if settings.preference.reasoning_effort.is_some()
        && model
            .router
            .catalog()
            .capability_support(model_id, model::Capability::Reasoning)
            == model::CapabilitySupport::Unsupported
    {
        return Err(unsupported(
            "candidate model does not support the recorded reasoning setting",
        ));
    }
    Ok(Arc::new(Frozen { model, settings }))
}

struct Frozen {
    model: Arc<RoutedModel>,
    settings: BenchmarkModelSettings,
}

impl Frozen {
    fn invocation(&self, invocation: &AgentInvocationContext) -> AgentInvocationContext {
        AgentInvocationContext {
            model_preference: Some(self.settings.preference.clone()),
            ..invocation.clone()
        }
    }
}

#[async_trait]
impl AgentModel for Frozen {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        format!(
            "{}/{}",
            self.settings.preference.profile,
            self.settings
                .preference
                .model
                .as_deref()
                .unwrap_or_default()
        )
    }

    fn catalog_context_window(&self, selected_model: &str) -> Option<u64> {
        self.model.catalog_context_window(selected_model)
    }

    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.model
            .complete_request(
                &self.invocation(invocation),
                role,
                messages,
                tools,
                None,
                None,
                Some(&self.settings),
            )
            .await
    }

    async fn complete_streaming(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
        bus: &EventBus,
    ) -> Result<ChatResponse, RuntimeError> {
        self.model
            .complete_request(
                &self.invocation(invocation),
                role,
                messages,
                tools,
                Some(bus),
                None,
                Some(&self.settings),
            )
            .await
    }
}
