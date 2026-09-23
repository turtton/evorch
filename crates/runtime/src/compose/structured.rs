//! Negotiate structured output without changing the selected provider or prompt.
use event_bus::EventBus;
use providers::{ChatRequest, ChatResponse, ProviderError};
use routing::ComposedProvider;

pub(super) async fn send(
    provider: &ComposedProvider,
    request: &mut ChatRequest,
    bus: Option<&EventBus>,
) -> Result<ChatResponse, ProviderError> {
    if !provider.client.supports_structured_output() {
        request.output_schema = None;
    }
    let result = send_once(provider, request, bus).await;
    if request.output_schema.is_some()
        && result
            .as_ref()
            .is_err_and(ProviderError::is_structured_output_unsupported)
    {
        // Only an explicit unsupported-format error permits a single downgrade.
        // Keep messages, generation settings, routing and cache affinity identical.
        // Never retry a successful verdict, malformed answer or unrelated failure here.
        tracing::info!("structured output unsupported; retrying with prompt-only output");
        request.output_schema = None;
        return send_once(provider, request, bus).await;
    }
    result
}

async fn send_once(
    provider: &ComposedProvider,
    request: &ChatRequest,
    bus: Option<&EventBus>,
) -> Result<ChatResponse, ProviderError> {
    match bus {
        Some(bus) => {
            provider
                .client
                .send_streaming(&provider.auth, request, bus)
                .await
        }
        None => provider.client.send(&provider.auth, request).await,
    }
}
