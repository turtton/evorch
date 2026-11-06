//! Wire decoder and input-derived cache independent of runtime cache observation.
use super::*;
use mock_openai::claude_contract::claude_model_input;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};
#[derive(Default)]
struct State {
    script: VecDeque<Step>,
    requests: Vec<Value>,
    prompts: Vec<Vec<u8>>,
}
#[derive(Clone, Default)]
struct Endpoint(Arc<Mutex<State>>);
impl Respond for Endpoint {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        // Verify the subscription fingerprint/checksum and normalize only the
        // documented moving breakpoint, not arguments or tool definitions.
        let input = claude_model_input(&request.body).unwrap();
        let mut normalized = input.clone();
        for message in normalized["messages"].as_array_mut().unwrap() {
            for block in message["content"].as_array_mut().unwrap() {
                if block.get("cache_control") == Some(&json!({"type":"ephemeral"})) {
                    block.as_object_mut().unwrap().remove("cache_control");
                }
            }
        }
        let messages = normalized
            .as_object_mut()
            .unwrap()
            .remove("messages")
            .unwrap();
        normalized.as_object_mut().unwrap().remove("stream");
        let mut prompt = serde_json::to_vec(&normalized).unwrap();
        for message in messages.as_array().unwrap() {
            prompt.extend(serde_json::to_vec(message).unwrap());
            prompt.push(b'\n');
        }
        let mut state = self.0.lock().unwrap();
        let cached = state
            .prompts
            .iter()
            .map(|old| old.iter().zip(&prompt).take_while(|(a, b)| a == b).count())
            .max()
            .unwrap_or(0) as u64;
        let total = prompt.len() as u64;
        let index = state.requests.len();
        state.requests.push(input);
        state.prompts.push(prompt);
        let (block, reason) = match state.script.pop_front().expect("unexpected Claude request") {
            Step::Call(name) => (
                json!({"type":"tool_use","id":format!("call-{index}"),"name":format!("_mcp__evorch__{name}"),"input":{"index":index}}),
                "tool_use",
            ),
            Step::Text(text) => (json!({"type":"text","text":text}), "end_turn"),
        };
        let events = [
            (
                "message_start",
                json!({"type":"message_start","message":{"id":format!("msg-{index}"),"type":"message","role":"assistant","model":CLAUDE_MODEL,"content":[],"usage":{"input_tokens":total-cached,"output_tokens":0,"cache_read_input_tokens":cached}}}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":block}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            (
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":{"output_tokens":1}}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ];
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(
                events
                    .into_iter()
                    .map(|(event, body)| format!("event: {event}\ndata: {body}\n\n"))
                    .collect::<String>(),
            )
    }
}
pub struct Claude {
    server: MockServer,
    endpoint: Endpoint,
}
impl Claude {
    pub async fn new() -> Self {
        let server = MockServer::start().await;
        let endpoint = Endpoint::default();
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data":[{"id":CLAUDE_MODEL}],"has_more":false})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("authorization", "Bearer fixture-mcp-token"))
            .respond_with(endpoint.clone())
            .mount(&server)
            .await;
        Self { server, endpoint }
    }
    pub fn uri(&self) -> String {
        self.server.uri()
    }
    pub fn enqueue(&self, steps: Vec<Step>) {
        self.endpoint.0.lock().unwrap().script.extend(steps);
    }
    pub fn requests(&self) -> Vec<Value> {
        self.endpoint.0.lock().unwrap().requests.clone()
    }
    pub fn assert_drained(&self) {
        assert!(self.endpoint.0.lock().unwrap().script.is_empty());
    }
}
