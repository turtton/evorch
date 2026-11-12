//! Pure checkpoint compatibility checks. Shared affinity is never changed here.
use super::*;

pub(super) struct RecoveryOutputLimit {
    pub(super) profile: String,
    pub(super) model: String,
    pub(super) limit: Option<u64>,
}

impl RoutedModel {
    pub(super) fn validate_recovery_checkpoint(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        pins: &[crate::RoutePinRecord],
        input: Option<&crate::quiesce::ProviderInput>,
    ) -> Result<(), RuntimeError> {
        let incompatible = || model_error("current route/settings differ from recovery checkpoint");
        let mut scratch = self
            .affinity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut seen = std::collections::BTreeSet::new();
        for pin in pins {
            let key = normalize_chain_key(&pin.chain);
            if !seen.insert(key.clone()) {
                return Err(incompatible());
            }
            let (name, category) = key
                .split_once('/')
                .map_or((key.as_str(), None), |(r, c)| (r, Some(c)));
            let pin_role = [
                Role::Orchestrator,
                Role::Explorer,
                Role::Worker,
                Role::Reviewer,
                Role::WebResearcher,
                Role::Planner,
                Role::Advisor,
                Role::MultimodalLooker,
                Role::Evaluator,
            ]
            .into_iter()
            .find(|role| role_key(*role) == name)
            .ok_or_else(incompatible)?;
            let binding = self
                .agents
                .binding_for(role_key(pin_role), category)
                .map_err(model_error)?;
            let chain = self
                .router_for(false)
                .chain(key.clone(), &binding.models)
                .map_err(model_error)?;
            self.ensure_chat_chain(&chain)?;
            if !chain
                .members()
                .iter()
                .any(|m| m.target.profile == pin.profile && m.target.model_id == pin.model)
            {
                return Err(incompatible());
            }
            if let Some(current) = scratch.pinned(&invocation.run_id, &key)
                && (current.profile != pin.profile || current.model != pin.model)
            {
                return Err(incompatible());
            }
            scratch.pin(
                &invocation.run_id,
                &key,
                routing::RoutePin {
                    position: pin.position,
                    profile: pin.profile.clone(),
                    model: pin.model.clone(),
                },
            );
        }
        let Some(input) = input else {
            return Ok(());
        };
        let (route, generation, reasoning) = if invocation.model_preference.is_some() {
            // The explicit-selection branch does not touch affinity.
            let resolved = self.resolve_invocation(invocation, role, &input.request.tools)?;
            (
                resolved.route,
                resolved.generation,
                resolved.reasoning_effort,
            )
        } else {
            let (binding, choice) = self.resolve_binding_in(
                &mut scratch,
                &invocation.run_id,
                role,
                invocation.category.as_deref(),
                !input.request.tools.is_empty(),
            )?;
            let member = choice.chain.member(choice.position);
            (
                member.target.clone(),
                binding.generation,
                member.reasoning_effort.clone(),
            )
        };
        let provider = self
            .providers
            .get(&route.profile)
            .ok_or_else(incompatible)?;
        let (model, speed) = config::types::provider::parse_model_speed(&route.model_id);
        let affinity = if provider.profile.api_protocol == model::ApiProtocol::OpenAiCodexResponses
        {
            providers::provider::codex::run_cache_affinity(&invocation.run_id)
        } else {
            invocation.run_id.clone()
        };
        if route.profile != input.profile
            || provider.profile.api_protocol != input.protocol
            || provider.profile.edit_tool_for(&route.model_id) != input.edit_tool
            || model != input.request.model
            || affinity != input.affinity_key
            || generation.temperature != input.request.temperature
            || reasoning != input.request.reasoning_effort
            || replay_service_tier(speed) != input.request.service_tier
            || self.checkpoint_output_limit(&invocation.run_id, &route, generation.max_tokens)?
                != input.request.max_tokens
        {
            return Err(incompatible());
        }
        Ok(())
    }
}
