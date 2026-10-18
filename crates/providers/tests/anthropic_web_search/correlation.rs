use std::sync::atomic::{AtomicUsize, Ordering};

use providers::ProviderAuth;
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::support::*;

#[tokio::test]
async fn failed_search_content_does_not_advance_subscription_correlation() {
    for invalid_content in [json!([message()]), json!([results("srv_orphan")])] {
        let server = MockServer::start().await;
        let sequence = AtomicUsize::new(0);
        Mock::given(method("POST"))
            .respond_with(move |_: &wiremock::Request| {
                let mut body = success();
                let (request_id, message_id) = match sequence.fetch_add(1, Ordering::SeqCst) {
                    0 => ("req_first", "msg_first"),
                    1 => {
                        body["content"] = invalid_content.clone();
                        ("req_invalid", "msg_invalid")
                    }
                    _ => ("req_last", "msg_last"),
                };
                body["id"] = json!(message_id);
                ResponseTemplate::new(200)
                    .insert_header("request-id", request_id)
                    .set_body_json(body)
            })
            .expect(3)
            .mount(&server)
            .await;
        let client = client(&server, true, None);
        let search = client.hosted_web_search().unwrap();
        let auth = ProviderAuth::new("unused");
        search.search(&auth, &request()).await.unwrap();
        search.search(&auth, &request()).await.unwrap_err();
        search.search(&auth, &request()).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        for request in &requests[1..] {
            let body: Value = request.body_json().unwrap();
            assert_eq!(body["diagnostics"]["previous_message_id"], "msg_first");
            assert!(
                body["system"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("cc_prev_req=req_first;")
            );
            mock_openai::claude_contract::claude_model_input(&request.body).unwrap();
        }
    }
}
