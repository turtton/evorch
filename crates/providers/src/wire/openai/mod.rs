//! OpenAI Chat Completions の wire 形式を扱います。

mod request;
mod response;
mod response_types;
mod stream;
mod types;

pub use request::{from_wire_messages, to_wire_request};
pub use response::{from_wire_response, to_finish_reason};
pub use response_types::{
    WireChatResponse, WireChoice, WirePromptTokensDetails, WireResponseMessage, WireStreamChoice,
    WireStreamChunk, WireStreamDelta, WireStreamFunction, WireStreamToolCall, WireUsage,
};
pub use stream::OpenAiStreamInterpreter;
pub use types::{
    WireChatRequest, WireContent, WireFunction, WireFunctionDefinition, WireMessage,
    WireStreamOptions, WireTextPart, WireTool, WireToolCall,
};

#[cfg(test)]
mod tests;
