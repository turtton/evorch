//! Image chains share routing health/fallback, but never pin the chat model.
use super::*;
use providers::{ImageGenerationError, ImageGenerationRequest};

impl RoutedModel {
    pub(super) async fn generate_image_request(
        &self,
        invocation: &AgentInvocationContext,
        mut request: ImageGenerationRequest,
    ) -> Result<crate::image_generation::GeneratedImage, String> {
        let entries = self.agents.image_generator_chain();
        if entries.is_empty() {
            return Err("image generator chain is empty".into());
        }
        let chain = self
            .router
            .chain("image_generator", entries)
            .map_err(|e| e.to_string())?;
        // Validate the entire chain before sending, just as for chat chains.
        for member in chain.members() {
            let target = &member.target;
            if self.model_kind(&target.profile, &target.model_id)
                != config::ModelKind::ImageGeneration
            {
                return Err(format!(
                    "image generator requires image_generation models: {}/{}",
                    target.profile, target.model_id
                ));
            }
            if self
                .providers
                .get(&target.profile)
                .and_then(|p| p.client.image_generation())
                .is_none()
            {
                return Err(format!(
                    "profile {} has no image generation capability",
                    target.profile
                ));
            }
        }
        let mut scratch = SessionAffinity::default();
        let mut position = self
            .router
            .resolve_chain(&mut scratch, &invocation.run_id, &chain)
            .map_err(|e| e.to_string())?;
        let mut cycle = None;
        request.observation = Some(ObservationContext {
            run_id: invocation.run_id.clone(),
            turn: invocation.turn.clone(),
            purpose: event_bus::RequestPurpose::ImageGeneration,
        });
        loop {
            let route = &chain.member(position).target;
            let provider = &self.providers[&route.profile];
            let capability = provider
                .client
                .image_generation()
                .ok_or("image generation capability is unavailable")?;
            request.model = config::types::provider::parse_model_speed(&route.model_id)
                .0
                .into();
            match capability.generate(&provider.auth, &request).await {
                Ok(response) => {
                    return Ok(crate::image_generation::GeneratedImage {
                        response,
                        profile: route.profile.clone(),
                        model: request.model,
                    });
                }
                Err(ImageGenerationError::Rejected(reason)) => return Err(reason),
                Err(ImageGenerationError::Provider(error)) => {
                    let failure =
                        routing::FailureKind::classify(&error, provider.profile.provider_type);
                    self.record_cooldown(
                        route,
                        failure,
                        routing::retry_after(&error),
                        &invocation.run_id,
                        None,
                    );
                    // Unlike chat, the image packet explicitly includes unsupported endpoints.
                    let step = FallbackAttempt {
                        failed: position,
                        from: route,
                        failure,
                        transient: routing::is_transient(&error),
                        retry_after: routing::retry_after(&error),
                    };
                    let Some(next) = self
                        .advance_fallback(&self.router, &chain, &mut cycle, step, invocation, None)
                        .await
                    else {
                        // Provider bodies may contain credentials or echoed reference images.
                        return Err(
                            "image generation failed; all available candidates exhausted".into(),
                        );
                    };
                    position = next;
                }
            }
        }
    }
}
