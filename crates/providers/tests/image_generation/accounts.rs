use providers::provider::codex::oauth::CODEX_CLIENT_ID;
use providers::{InlineImage, ProviderAuth};
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, ResponseTemplate};

use super::*;

#[tokio::test]
async fn ac2_codex_images_share_refresh_account_and_session_headers() {
    let server = MockServer::start().await;
    let store = Arc::new(InMemoryTokenStore::new());
    let jwt = |exp| {
        format!("e30.{}.signature",base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json!({"exp":exp,"https://api.openai.com/auth":{"chatgpt_account_id":"selected-account"}}).to_string()))
    };
    store
        .save(&TokenBundle {
            access_token: "expired-access".into(),
            refresh_token: "secret-refresh".into(),
            id_token: jwt(0),
        })
        .unwrap();
    Mock::given(method("POST")).and(path("/oauth/token"))
        .and(body_json(json!({"client_id":CODEX_CLIENT_ID,"grant_type":"refresh_token","refresh_token":"secret-refresh","refresh_reason":"proactive_expiry"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"refreshed-access","refresh_token":"new-refresh","id_token":jwt(u64::MAX)}))).expect(1).mount(&server).await;
    for edit in [false, true] {
        Mock::given(method("POST"))
            .and(path(image_path(true, edit)))
            .and(header("authorization", "Bearer refreshed-access"))
            .and(header("chatgpt-account-id", "selected-account"))
            .respond_with(ResponseTemplate::new(200).set_body_json(success()))
            .expect(1)
            .mount(&server)
            .await;
    }
    let client = CodexClient::with_config(
        CodexConfig {
            base_url: server.uri(),
            auth_base_url: server.uri(),
            ..Default::default()
        },
        store.clone(),
    )
    .unwrap();
    let capability = client.image_generation().unwrap();
    let auth = ProviderAuth::new("must-not-be-used");
    let generation = request();
    let mut edit = request();
    edit.reference_images = vec![InlineImage {
        media_type: "image/png".into(),
        data: base64::engine::general_purpose::STANDARD.encode(png()),
    }];
    let (generated, edited) = tokio::join!(
        capability.generate(&auth, &generation),
        capability.generate(&auth, &edit)
    );
    generated.unwrap();
    edited.unwrap();
    assert_eq!(
        store.load().unwrap().unwrap().access_token,
        "refreshed-access"
    );
    let requests = server.received_requests().await.unwrap();
    let images = requests
        .iter()
        .filter(|r| r.url.path().contains("/images/"))
        .collect::<Vec<_>>();
    assert_eq!(images.len(), 2);
    // Both calls belong to the "image-run" run: every identifier is that run's UUIDv5.
    let run_id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, b"image-run").to_string();
    for image in images {
        for name in ["session-id", "thread-id", "x-client-request-id"] {
            assert_eq!(image.headers[name], run_id.as_str(), "{name}");
        }
    }
}
