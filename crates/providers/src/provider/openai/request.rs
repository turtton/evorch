use super::ChatCompletionsClient;
use crate::message::ChatRequest;
use crate::wire::openai::{WireChatRequest, to_wire_request};

impl ChatCompletionsClient {
    pub(super) fn wire_request(&self, request: &ChatRequest, streaming: bool) -> WireChatRequest {
        let mut wire = to_wire_request(request, streaming);
        if self.prompt_cache_key {
            wire.prompt_cache_key = request
                .observation
                .as_ref()
                .map(|context| context.run_id.clone());
        }
        wire
    }
}
