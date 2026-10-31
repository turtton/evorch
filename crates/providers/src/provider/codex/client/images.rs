use async_trait::async_trait;

use super::{
    AuthorizedCodexToken, CodexClient, PROVIDER_LABEL, reject_second_unauthorized, scope_probe,
};
use crate::http::map_request_error;
use crate::image_generation::{self, IMAGE_TIMEOUT};
use crate::{
    ImageGeneration, ImageGenerationError, ImageGenerationRequest, ImageGenerationResponse,
    ProviderAuth, ProviderError,
};

#[async_trait]
impl ImageGeneration for CodexClient {
    fn provider_name(&self) -> &'static str {
        PROVIDER_LABEL
    }

    async fn generate(
        &self,
        _auth: &ProviderAuth,
        request: &ImageGenerationRequest,
    ) -> Result<ImageGenerationResponse, ImageGenerationError> {
        let body = image_generation::wire_request(request, true)?;
        let endpoint = image_generation::endpoint(&self.endpoint, request);
        let run_id = super::run_id_of(request.observation.as_ref());
        let mut observer = image_generation::observer(
            self.event_bus.clone(),
            PROVIDER_LABEL,
            self.profile.clone(),
            request,
        );
        observer.emit_started();
        image_generation::execute(
            request,
            self.event_bus.clone(),
            PROVIDER_LABEL,
            &mut observer,
            async |observer| {
                // The compaction client already permits five minutes without response bytes.
                let token = self.session.current().await?;
                let response = self
                    .send_image_request(&endpoint, &body, &token, run_id)
                    .await?;
                if response.status() != reqwest::StatusCode::UNAUTHORIZED {
                    return Ok(response);
                }
                drop(response);
                let fresh = self
                    .recover_unauthorized(&scope_probe(&request.model), &token, observer)
                    .await?;
                observer.emit_started();
                let response = self
                    .send_image_request(&endpoint, &body, &fresh, run_id)
                    .await?;
                reject_second_unauthorized(response, observer)
            },
        )
        .await
    }
}

impl CodexClient {
    async fn send_image_request(
        &self,
        endpoint: &str,
        body: &impl serde::Serialize,
        token: &AuthorizedCodexToken,
        run_id: Option<&str>,
    ) -> Result<reqwest::Response, ProviderError> {
        self.authenticated_request_with_token(&self.compaction_http_client, endpoint, token, run_id)
            .await
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(IMAGE_TIMEOUT)
            .json(body)
            .send()
            .await
            .map_err(map_request_error)
    }
}
