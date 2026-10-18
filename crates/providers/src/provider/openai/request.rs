use super::{ChatCompletionsClient, OutputLimitField};
use crate::message::ChatRequest;
use crate::wire::openai::{WireChatRequest, to_wire_request};

impl ChatCompletionsClient {
    pub(crate) fn wire_request(&self, request: &ChatRequest, streaming: bool) -> WireChatRequest {
        let mut wire = to_wire_request(request, streaming);
        if matches!(
            self.output_limit_field,
            OutputLimitField::MaxCompletionTokens
        ) {
            wire.max_completion_tokens = wire.max_tokens.take();
        }
        if self.prompt_cache_key {
            wire.prompt_cache_key = request
                .observation
                .as_ref()
                .map(|context| context.run_id.clone());
        }
        wire
    }
}
