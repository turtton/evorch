use async_trait::async_trait;

use super::{CodexClient, PROVIDER_LABEL};
use crate::http::map_request_error;
use crate::image_generation::{self, IMAGE_TIMEOUT};
use crate::{
    ImageGeneration, ImageGenerationError, ImageGenerationRequest, ImageGenerationResponse,
    ProviderAuth,
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
            async {
                // The compaction client already permits five minutes without response bytes.
                self.authenticated_request(&self.compaction_http_client, &endpoint)
                    .await?
                    .header(reqwest::header::ACCEPT, "application/json")
                    .timeout(IMAGE_TIMEOUT)
                    .json(&body)
                    .send()
                    .await
                    .map_err(map_request_error)
            },
        )
        .await
    }
}
