//! Optional, non-streaming image generation/editing capability.

mod response;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use event_bus::{EventBus, RequestPurpose};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::http::UsageEmitter;
use crate::observe::AttemptObserver;
use crate::{FinishReason, ObservationContext, ProviderAuth, ProviderError, Usage};

pub(crate) const IMAGE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
pub(crate) const MAX_RESPONSE_BYTES: usize = 48 * 1024 * 1024;

/// Base64-encoded reference image. Local paths never enter provider requests.
#[derive(Clone)]
pub struct InlineImage {
    pub media_type: String,
    pub data: String,
}

/// Requested PNG background; omission leaves the choice to the endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageBackground {
    Opaque,
    Transparent,
}

/// Empty references generate an image; 1–5 references select the edits endpoint.
#[derive(Clone, Default)]
pub struct ImageGenerationRequest {
    pub model: String,
    pub prompt: String,
    pub reference_images: Vec<InlineImage>,
    pub background: Option<ImageBackground>,
    pub size: Option<String>,
    pub quality: Option<String>,
    /// Purpose is always overridden to `ImageGeneration`, preserving run/turn IDs.
    pub observation: Option<ObservationContext>,
    /// Called once after a fully validated success. Response usage is metadata only.
    pub usage_sink: Option<Arc<dyn Fn(Usage) + Send + Sync>>,
}

/// One validated PNG and its already-accounted usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageGenerationResponse {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub background: Option<ImageBackground>,
    pub usage: Usage,
    pub request_id: Option<String>,
}

/// Rejected input/content must not fall back to another provider.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ImageGenerationError {
    #[error("image generation rejected: {0}")]
    Rejected(String),
    #[error("image generation quota exceeded")]
    Quota { retry_after: Option<Duration> },
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

/// Optional capability discovered through [`crate::ProviderClient`].
#[async_trait]
pub trait ImageGeneration: Send + Sync {
    fn provider_name(&self) -> &'static str;

    async fn generate(
        &self,
        auth: &ProviderAuth,
        request: &ImageGenerationRequest,
    ) -> Result<ImageGenerationResponse, ImageGenerationError>;
}

pub(crate) fn wire_request(
    request: &ImageGenerationRequest,
    codex: bool,
) -> Result<Value, ImageGenerationError> {
    use base64::Engine;
    if request.prompt.trim().is_empty() || request.model.trim().is_empty() {
        return Err(ImageGenerationError::Rejected(
            "model and prompt are required".into(),
        ));
    }
    if request.reference_images.len() > 5 {
        return Err(ImageGenerationError::Rejected(
            "at most 5 reference images are allowed".into(),
        ));
    }
    let mut body = json!({"model": request.model, "prompt": request.prompt});
    if let Some(background) = request.background {
        body["background"] = json!(background);
    }
    for (key, value) in [("size", &request.size), ("quality", &request.quality)] {
        if let Some(value) = value.as_deref().or(codex.then_some("auto")) {
            body[key] = json!(value);
        }
    }
    if !codex {
        body["output_format"] = json!("png");
    }
    if !request.reference_images.is_empty() {
        let mut images = Vec::new();
        for image in &request.reference_images {
            if !matches!(
                image.media_type.as_str(),
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            ) || image.data.is_empty()
                || base64::engine::general_purpose::STANDARD
                    .decode(&image.data)
                    .is_err()
            {
                return Err(ImageGenerationError::Rejected(
                    "invalid inline reference image".into(),
                ));
            }
            images.push(
                json!({"image_url": format!("data:{};base64,{}", image.media_type, image.data)}),
            );
        }
        body["images"] = json!(images);
    }
    Ok(body)
}

pub(crate) fn endpoint(responses_endpoint: &str, request: &ImageGenerationRequest) -> String {
    let base = responses_endpoint
        .strip_suffix("/responses")
        .expect("Responses endpoint");
    let operation = if request.reference_images.is_empty() {
        "generations"
    } else {
        "edits"
    };
    format!("{base}/images/{operation}")
}

pub(crate) fn observer(
    bus: Option<Arc<EventBus>>,
    provider: &str,
    profile: Option<String>,
    request: &ImageGenerationRequest,
) -> AttemptObserver {
    AttemptObserver::new(
        bus,
        provider,
        profile,
        "openai-images",
        request.model.clone(),
        false,
        request.observation.clone(),
    )
    .with_purpose(RequestPurpose::ImageGeneration)
}

/// Covers authentication/refresh, HTTP headers/body and decoding with one deadline.
/// No internal retries: a generation may already have consumed quota.
pub(crate) async fn execute(
    request: &ImageGenerationRequest,
    bus: Option<Arc<EventBus>>,
    provider: &str,
    observer: &mut AttemptObserver,
    send: impl AsyncFnOnce(&mut AttemptObserver) -> Result<reqwest::Response, ProviderError>,
) -> Result<ImageGenerationResponse, ImageGenerationError> {
    let result = tokio::time::timeout(IMAGE_TIMEOUT, async {
        let response = send(&mut *observer).await.map_err(response::sanitize)?;
        response::decode(response).await
    })
    .await
    .unwrap_or_else(|_| Err(ProviderError::Timeout.into()));
    match &result {
        Ok(response) => {
            if let Some(sink) = &request.usage_sink {
                sink(response.usage);
            }
            UsageEmitter::new(bus, provider).emit_usage(&request.model, &response.usage);
            observer.emit_completed(&response.usage, FinishReason::Stop);
        }
        Err(ImageGenerationError::Quota { .. }) => observer.emit_failed(&ProviderError::Http {
            status: 402,
            body: "image generation quota exceeded".into(),
        }),
        Err(ImageGenerationError::Provider(error)) => observer.emit_failed(error),
        Err(ImageGenerationError::Rejected(_)) => observer.emit_failed(&ProviderError::Http {
            status: 400,
            body: "image request rejected".into(),
        }),
    }
    result
}
