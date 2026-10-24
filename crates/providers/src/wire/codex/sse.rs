//! Codex の SSE は公式 OpenAI と共通の Responses interpreter で解釈します。

pub use crate::wire::responses::ResponsesStreamInterpreter as CodexStreamInterpreter;

#[cfg(test)]
mod tests;
