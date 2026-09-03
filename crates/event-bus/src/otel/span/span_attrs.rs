use std::fmt;

use super::{SpanAttribute, SpanAttributeValue};

/// span attribute キーの closed whitelist (辞書順)。
pub const SPAN_ATTRIBUTE_WHITELIST: [&str; 18] = [
    "error.type",
    "evorch.agent.name",
    "evorch.agent.role",
    "evorch.agent_run.id",
    "evorch.delegation.depth",
    "evorch.parent_agent_run.id",
    "evorch.request.id",
    "evorch.session.id",
    "evorch.task.id",
    "gen_ai.agent.name",
    "gen_ai.operation.name",
    "gen_ai.provider.name",
    "gen_ai.request.model",
    "gen_ai.response.finish_reasons",
    "gen_ai.tool.call.id",
    "gen_ai.tool.name",
    "gen_ai.usage.input_tokens",
    "gen_ai.usage.output_tokens",
];

const RAW_CONTENT_KEY_PARTS: [&str; 11] = [
    "gen_ai.prompt",
    "gen_ai.completion",
    "input.messages",
    "output.messages",
    "content",
    "message",
    "body",
    "sse",
    "credential",
    "token",
    "api_key",
];
const OPERATION_NAMES: [&str; 3] = ["chat", "invoke_agent", "execute_tool"];
const AGENT_ROLES: [&str; 4] = ["orchestrator", "explorer", "worker", "reviewer"];
const PROVIDER_NAMES: [&str; 5] = [
    "anthropic",
    "openai",
    "openai-compatible",
    "evorch",
    "other",
];
const ERROR_TYPES: [&str; 13] = [
    "rate_limited",
    "http",
    "timeout",
    "invalid_response",
    "transport",
    "server",
    "quota",
    "auth",
    "other",
    "agent_run_error",
    "tool_error",
    "session_failed",
    "span_budget_evicted",
];

/// span 属性の closed-domain 違反。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpanAttributeViolation {
    /// raw content または credential を示す denylist key。
    RawContentKey { key: String },
    /// whitelist 外の key。
    UnknownKey { key: String },
    /// key 固有の closed domain 外の値。
    InvalidValue { key: String },
}

impl fmt::Display for SpanAttributeViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RawContentKey { key } => write!(formatter, "raw-content attribute key `{key}`"),
            Self::UnknownKey { key } => write!(formatter, "unknown span attribute key `{key}`"),
            Self::InvalidValue { key } => {
                write!(formatter, "invalid value for span attribute `{key}`")
            }
        }
    }
}

impl std::error::Error for SpanAttributeViolation {}

/// span 属性列を whitelist と key 固有の closed domain で検査する。
pub fn validate_span_attributes(
    attributes: &[SpanAttribute],
) -> Result<(), SpanAttributeViolation> {
    for attribute in attributes {
        validate_attribute(attribute)?;
    }
    Ok(())
}

pub(super) fn validate_attribute(attribute: &SpanAttribute) -> Result<(), SpanAttributeViolation> {
    let key = attribute.key.as_str();
    if is_raw_content_key(key) {
        return Err(SpanAttributeViolation::RawContentKey {
            key: attribute.key.clone(),
        });
    }
    if SPAN_ATTRIBUTE_WHITELIST.binary_search(&key).is_err() {
        return Err(SpanAttributeViolation::UnknownKey {
            key: attribute.key.clone(),
        });
    }
    let valid = match key {
        "gen_ai.operation.name" => string_in(&attribute.value, &OPERATION_NAMES),
        "evorch.agent.role" => string_in(&attribute.value, &AGENT_ROLES),
        "evorch.delegation.depth" => {
            matches!(attribute.value, SpanAttributeValue::I64(value) if (0..=99).contains(&value))
        }
        "gen_ai.provider.name" => string_in(&attribute.value, &PROVIDER_NAMES),
        "error.type" => string_in(&attribute.value, &ERROR_TYPES),
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(SpanAttributeViolation::InvalidValue {
            key: attribute.key.clone(),
        })
    }
}

fn is_raw_content_key(key: &str) -> bool {
    RAW_CONTENT_KEY_PARTS
        .iter()
        .any(|part| key.contains(part) && !key.starts_with("gen_ai.usage."))
}

fn string_in(value: &SpanAttributeValue, domain: &[&str]) -> bool {
    matches!(value, SpanAttributeValue::Str(value) if domain.contains(&value.as_str()))
}
