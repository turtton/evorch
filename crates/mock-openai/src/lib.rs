//! Deterministic OpenAI-compatible fixtures with SSE/JSON dispatch by request stream flag.

pub mod scenario;
pub mod server;

pub use scenario::ScriptedResponse;
pub use server::{RecordedRequest, StreamingMockOpenAi, WriteMode};
