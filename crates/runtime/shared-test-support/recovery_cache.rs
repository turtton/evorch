use serde_json::json;
pub(crate) async fn assert_wait_cache(
    before: &providers::ChatRequest,
    after: &providers::ChatRequest,
    expected_outputs: &[&str],
) {
    use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
    use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
    use providers::wire::responses::{ResponsesDialect, ResponsesRequestOptions};
    let options = |dialect| ResponsesRequestOptions {
        dialect,
        prompt_cache_key: Some("stable-affinity".into()),
        codex_model: None,
    };
    for dialect in [
        None,
        Some(ResponsesDialect::OpenAi),
        Some(ResponsesDialect::Codex),
    ] {
        let mock = StreamingMockOpenAi::spawn_with_prompt_cache(
            (0..2)
                .map(|_| ScriptedResponse::text_stream("response", "mock-model", ["ok"]))
                .collect(),
        );
        let encode = |request: &providers::ChatRequest| match dialect {
            None => serde_json::to_value(providers::wire::openai::to_wire_request(request, false))
                .unwrap(),
            Some(dialect) => {
                let mut value = serde_json::to_value(providers::wire::responses::to_wire_request(
                    request,
                    &options(dialect),
                ))
                .unwrap();
                value["stream"] = json!(false);
                value
            }
        };
        let client = reqwest::Client::new();
        let path = if dialect.is_none() {
            "chat/completions"
        } else {
            "responses"
        };
        let mut usage = Vec::new();
        for request in [before, after] {
            let body = client
                .post(format!("{}/{path}", mock.base_url()))
                .json(&encode(request))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .text()
                .await
                .unwrap();
            let observed = if dialect.is_none() {
                serde_json::from_str::<serde_json::Value>(&body).unwrap()["usage"].clone()
            } else {
                body.lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
                    .find(|event| event["type"] == "response.completed")
                    .expect("mock emits a terminal response")["response"]["usage"]
                    .clone()
            };
            usage.push(observed);
        }
        let captured = mock.recorded_requests();
        assert_eq!(captured.len(), 2);
        assert_append_only(
            if dialect.is_none() {
                CacheProtocol::OpenAi
            } else {
                CacheProtocol::Codex
            },
            &captured[0].body,
            &captured[1].body,
        )
        .unwrap();
        let (input_key, detail_key) = if dialect.is_none() {
            ("prompt_tokens", "prompt_tokens_details")
        } else {
            ("input_tokens", "input_tokens_details")
        };
        assert!(usage[0][input_key].as_u64().unwrap() > 0);
        assert_eq!(usage[1][detail_key]["cached_tokens"], usage[0][input_key]);
        let body = &captured[1].body;
        let outputs: Vec<_> = if dialect.is_none() {
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|item| item["tool_call_id"].as_str())
                .collect()
        } else {
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["type"] == "function_call_output")
                .filter_map(|item| item["call_id"].as_str())
                .collect()
        };
        assert_eq!(outputs, expected_outputs);
    }
}
