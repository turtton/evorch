mod accounts;
mod errors;
mod limits;
mod requests;

use std::io::Cursor;
use std::sync::Arc;

use base64::Engine;
use event_bus::EventBus;
use providers::provider::codex::tokens::{CodexTokenStore, InMemoryTokenStore, TokenBundle};
use providers::provider::codex::{CodexClient, CodexConfig};
use providers::provider::openai::{OpenAiClient, OpenAiConfig};
use providers::{ImageGenerationRequest, ObservationContext, ProviderClient};
use serde_json::{Value, json};
use wiremock::MockServer;

fn client(
    server: &MockServer,
    codex: bool,
    bus: Option<Arc<EventBus>>,
    suffix: &str,
) -> Box<dyn ProviderClient> {
    if codex {
        let store = Arc::new(InMemoryTokenStore::new());
        let claims = json!({"exp":u64::MAX,"https://api.openai.com/auth":{"chatgpt_account_id":"selected-account"}});
        store
            .save(&TokenBundle {
                access_token: "secret-session".into(),
                refresh_token: "secret-refresh".into(),
                id_token: format!(
                    "e30.{}.signature",
                    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string())
                ),
            })
            .unwrap();
        Box::new(
            CodexClient::with_config(
                CodexConfig {
                    base_url: format!("{}{suffix}", server.uri()),
                    auth_base_url: server.uri(),
                    event_bus: bus,
                    ..Default::default()
                },
                store,
            )
            .unwrap()
            .with_profile("selected-profile"),
        )
    } else {
        Box::new(
            OpenAiClient::new(OpenAiConfig {
                base_url: format!("{}/v1/", server.uri()),
                event_bus: bus,
                ..Default::default()
            })
            .unwrap()
            .with_profile("selected-profile"),
        )
    }
}

fn request() -> ImageGenerationRequest {
    ImageGenerationRequest {
        model: "gpt-image-2".into(),
        prompt: "draw a tree".into(),
        observation: Some(ObservationContext {
            attempts: None,
            run_id: "image-run".into(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn png() -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(2, 3)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

fn success() -> Value {
    json!({"created":123,"background":"transparent","data":[{"b64_json":base64::engine::general_purpose::STANDARD.encode(png()),"generation_id":"gen-1"}],"usage":{"input_tokens":42,"output_tokens":7,"total_tokens":49,"input_tokens_details":{"text_tokens":12,"image_tokens":30}}})
}

fn image_path(codex: bool, edit: bool) -> String {
    format!(
        "{}/images/{}",
        if codex { "/backend-api/codex" } else { "/v1" },
        if edit { "edits" } else { "generations" }
    )
}
