use async_trait::async_trait;

use super::{OPENAI_PROVIDER_LABEL, OpenAiClient};
use crate::http::map_request_error;
use crate::image_generation;
use crate::{
    ImageGeneration, ImageGenerationError, ImageGenerationRequest, ImageGenerationResponse,
    ProviderAuth,
};

#[async_trait]
impl ImageGeneration for OpenAiClient {
    fn provider_name(&self) -> &'static str {
        OPENAI_PROVIDER_LABEL
    }

    async fn generate(
        &self,
        auth: &ProviderAuth,
        request: &ImageGenerationRequest,
    ) -> Result<ImageGenerationResponse, ImageGenerationError> {
        let body = image_generation::wire_request(request, false)?;
        let endpoint = image_generation::endpoint(&self.endpoint, request);
        let mut observer = image_generation::observer(
            self.event_bus.clone(),
            OPENAI_PROVIDER_LABEL,
            self.profile.clone(),
            request,
        );
        observer.emit_started();
        image_generation::execute(
            request,
            self.event_bus.clone(),
            OPENAI_PROVIDER_LABEL,
            &mut observer,
            async |_| {
                self.image_http
                    .post(&endpoint)
                    .bearer_auth(&auth.api_key)
                    .header(reqwest::header::ACCEPT, "application/json")
                    .json(&body)
                    .send()
                    .await
                    .map_err(map_request_error)
            },
        )
        .await
    }
}
