//! Deterministic OpenAI-compatible fixtures with SSE/JSON dispatch by request stream flag.

pub mod cache_contract;
pub mod claude_contract;
pub mod codex_auth;
pub mod scenario;
pub mod server;

pub use scenario::{ReasoningItem, ScriptedResponse};
pub use server::{RecordedRequest, ResponseGate, StreamingMockOpenAi, WriteMode};
