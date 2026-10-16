//! Attempt metadata is deliberately outside model history and cache observation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use event_bus::RequestPurpose;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Serialize;

use crate::{ObservationContext, ProviderError, RequestTurnContext};

const MAX_SCOPES: usize = 128;
const MAX_INDEX: u64 = 10_000_000;

#[derive(Clone, Default)]
pub(crate) struct ClaudeTransportContext(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    generation: u64,
    scopes: HashMap<Scope, Previous>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Scope {
    session: String,
    device: String,
    purpose: RequestPurpose,
}

#[derive(Default)]
struct Previous {
    created: u64,
    last_used: u64,
    committed: u64,
    request_id: Option<String>,
    message_id: Option<String>,
}

pub(crate) struct ClaudeRequestContext {
    state: ClaudeTransportContext,
    scope: Scope,
    generation: u64,
    request_id: String,
    turn: Option<RequestTurnContext>,
    previous_request_id: Option<String>,
    previous_message_id: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct ClaudeDiagnostics<'a> {
    previous_message_id: Option<&'a str>,
}

impl ClaudeTransportContext {
    pub fn begin(
        &self,
        session: &str,
        device: &str,
        observation: Option<&ObservationContext>,
    ) -> Result<ClaudeRequestContext, ProviderError> {
        let turn = observation.and_then(|observation| observation.turn.as_ref());
        if let Some(turn) = turn {
            validate_turn(turn)?;
        }
        let scope = Scope {
            session: session.to_owned(),
            device: device.to_owned(),
            purpose: observation.map(|o| o.purpose).unwrap_or_default(),
        };
        let request_id = uuid::Uuid::new_v4().to_string();
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.generation = state
            .generation
            .checked_add(1)
            .ok_or_else(|| ProviderError::Request("Claude request generation exhausted".into()))?;
        let generation = state.generation;
        if !state.scopes.contains_key(&scope) && state.scopes.len() == MAX_SCOPES {
            let oldest = state
                .scopes
                .iter()
                .min_by_key(|(_, previous)| previous.last_used)
                .map(|(scope, _)| scope.clone());
            if let Some(oldest) = oldest {
                state.scopes.remove(&oldest);
            }
        }
        let previous = state
            .scopes
            .entry(scope.clone())
            .or_insert_with(|| Previous {
                created: generation,
                ..Previous::default()
            });
        previous.last_used = generation;
        Ok(ClaudeRequestContext {
            state: self.clone(),
            scope,
            generation,
            request_id,
            turn: turn.cloned(),
            previous_request_id: previous.request_id.clone(),
            previous_message_id: previous.message_id.clone(),
        })
    }
}

impl ClaudeRequestContext {
    pub fn apply_headers(&self, headers: &mut HeaderMap) -> Result<(), ProviderError> {
        headers.insert(
            "x-client-request-id",
            HeaderValue::from_str(&self.request_id).expect("generated UUID is a header value"),
        );
        headers.insert(
            "x-claude-code-request-class",
            HeaderValue::from_static(match self.scope.purpose {
                RequestPurpose::Agent => "main",
                RequestPurpose::Compaction => "compaction",
                _ => "auxiliary",
            }),
        );
        if let Some(turn) = &self.turn {
            headers.insert(
                "x-claude-code-prompt-id",
                HeaderValue::from_str(&turn.prompt_id).expect("validated UUID is a header value"),
            );
        }
        const DIAGNOSIS_BETA: &str = "cache-diagnosis-2026-04-07";
        let betas = headers
            .get("anthropic-beta")
            .map(HeaderValue::to_str)
            .transpose()
            .map_err(|_| ProviderError::Request("Claude beta header invalid".into()))?
            .unwrap_or_default();
        if !betas.split(',').any(|beta| beta.trim() == DIAGNOSIS_BETA) {
            let betas = if betas.is_empty() {
                DIAGNOSIS_BETA.to_owned()
            } else {
                format!("{betas},{DIAGNOSIS_BETA}")
            };
            headers.insert(
                "anthropic-beta",
                HeaderValue::from_str(&betas)
                    .map_err(|_| ProviderError::Request("Claude beta header invalid".into()))?,
            );
        }
        Ok(())
    }

    pub fn append_billing(&self, billing: &mut String) {
        use std::fmt::Write;

        if self.turn.as_ref().is_some_and(|turn| turn.is_subagent) {
            billing.push_str(" cc_is_subagent=true;");
        }
        if let Some(previous) = &self.previous_request_id {
            let _ = write!(billing, " cc_prev_req={previous};");
        }
        if let Some(turn) = &self.turn {
            let _ = write!(billing, " cc_prompt_id={};", turn.prompt_id);
            if let Some(origin) = &turn.origin {
                let _ = write!(billing, " cc_turn_origin={origin};");
            }
            if let (Some(prompt), Some(turn)) = (turn.prompt_index, turn.turn_index) {
                let _ = write!(billing, " cc_prompt_index={prompt}; cc_turn_index={turn};");
            }
        }
    }

    pub fn diagnostics(&self) -> ClaudeDiagnostics<'_> {
        ClaudeDiagnostics {
            previous_message_id: self.previous_message_id.as_deref(),
        }
    }

    /// Commit only after the response and canonical tool-event conversion succeed.
    pub fn complete(self, request_id: Option<&str>, message_id: Option<&str>) {
        let mut state = self
            .state
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(previous) = state.scopes.get_mut(&self.scope)
            && self.generation >= previous.created
            && self.generation > previous.committed
        {
            previous.committed = self.generation;
            previous.request_id = request_id
                .filter(|value| valid_provider_id(value, "req_", 36))
                .map(str::to_owned);
            previous.message_id = message_id
                .filter(|value| valid_provider_id(value, "msg_", 196))
                .map(str::to_owned);
        }
    }
}

fn validate_turn(turn: &RequestTurnContext) -> Result<(), ProviderError> {
    let valid_uuid = uuid::Uuid::parse_str(&turn.prompt_id).is_ok_and(|value| {
        value
            .hyphenated()
            .to_string()
            .eq_ignore_ascii_case(&turn.prompt_id)
    });
    let valid_origin = turn.origin.as_deref().is_none_or(|origin| {
        origin
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_lowercase)
            && origin.len() <= 32
            && origin
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
    });
    if !valid_uuid
        || !valid_origin
        || turn.prompt_index.is_some_and(|index| index > MAX_INDEX)
        || turn
            .turn_index
            .is_some_and(|index| !(1..=MAX_INDEX).contains(&index))
    {
        return Err(ProviderError::Request(
            "Claude request turn metadata invalid".into(),
        ));
    }
    Ok(())
}

fn valid_provider_id(value: &str, prefix: &str, max_suffix: usize) -> bool {
    value.starts_with(prefix)
        && value.len() > prefix.len()
        && value.len() <= prefix.len() + max_suffix
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

#[cfg(test)]
mod tests;
