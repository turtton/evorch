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
        let router = self
            .router
            .clone()
            .with_additional_health(Arc::clone(&self.image_health));
        let mut scratch = SessionAffinity::default();
        let mut position = router
            .resolve_chain(&mut scratch, &invocation.run_id, &chain)
            .map_err(|e| e.to_string())?;
        let mut cycle = None;
        request.observation = Some(ObservationContext {
            attempts: None,
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
                Err(error) => {
                    let (failure, retry_after, transient) = match &error {
                        ImageGenerationError::Quota { retry_after } => {
                            (routing::FailureKind::Quota, *retry_after, false)
                        }
                        ImageGenerationError::Provider(error) => (
                            routing::FailureKind::classify(error, provider.profile.provider_type),
                            routing::retry_after(error),
                            routing::is_transient(error),
                        ),
                        ImageGenerationError::Rejected(_) => unreachable!(),
                    };
                    self.record_cooldown(route, failure, retry_after, &invocation.run_id, None);
                    if !matches!(
                        failure,
                        routing::FailureKind::Auth
                            | routing::FailureKind::Quota
                            | routing::FailureKind::RateLimited
                    ) {
                        self.image_health.record_candidate_failure(
                            &route.profile,
                            &route.model_id,
                            std::time::Duration::from_secs(60),
                            std::time::Instant::now(),
                        );
                    }
                    // Unlike chat, the image packet explicitly includes unsupported endpoints.
                    let step = FallbackAttempt {
                        failed: position,
                        from: route,
                        failure,
                        transient,
                        retry_after,
                    };
                    let Some(next) = self
                        .advance_fallback(&router, &chain, &mut cycle, step, invocation, None)
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
