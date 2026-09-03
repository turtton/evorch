use std::time::SystemTime;

use crate::event::{ProviderEvent, ProviderFailureKind};

use super::state::{EndSpec, StartSpec, terminal_error};
use super::{SpanAction, SpanAttribute, SpanDropKind, SpanKey, SpanKind, SpanMapper, SpanStatus};

impl SpanMapper {
    pub(super) fn map_provider(
        &mut self,
        event: &ProviderEvent,
        at: SystemTime,
    ) -> Vec<SpanAction> {
        match event {
            ProviderEvent::RequestStarted { .. } => self.start_request(event, at),
            ProviderEvent::RequestCompleted {
                request_id,
                input_tokens,
                output_tokens,
                finish_reason,
                ..
            } => {
                let mut terminal = Vec::with_capacity(3);
                if let Ok(value) = i64::try_from(*input_tokens) {
                    terminal.push(SpanAttribute::new("gen_ai.usage.input_tokens", value));
                }
                if let Ok(value) = i64::try_from(*output_tokens) {
                    terminal.push(SpanAttribute::new("gen_ai.usage.output_tokens", value));
                }
                terminal.push(SpanAttribute::new(
                    "gen_ai.response.finish_reasons",
                    finish_reason.clone(),
                ));
                self.end_span(EndSpec {
                    key: SpanKey::Request {
                        request_id: request_id.clone(),
                    },
                    at,
                    status: SpanStatus::Unset,
                    terminal,
                })
            }
            ProviderEvent::RequestFailed {
                request_id,
                failure,
                ..
            } => self.end_span(EndSpec {
                key: SpanKey::Request {
                    request_id: request_id.clone(),
                },
                at,
                status: SpanStatus::Error,
                terminal: terminal_error(Some(failure_name(failure))),
            }),
            ProviderEvent::FirstTokenObserved { .. }
            | ProviderEvent::ProviderFallback { .. }
            | ProviderEvent::FallbackTriggered { .. } => Vec::new(),
        }
    }

    fn start_request(&mut self, event: &ProviderEvent, at: SystemTime) -> Vec<SpanAction> {
        let ProviderEvent::RequestStarted {
            request_id,
            provider,
            model,
            run_id,
            ..
        } = event
        else {
            return Vec::new();
        };
        let key = SpanKey::Request {
            request_id: request_id.clone(),
        };
        let Some(run_id) = run_id else {
            self.record_drop(SpanDropKind::MissingRunId, key, at);
            return Vec::new();
        };
        if !self.open.contains_key(&SpanKey::Agent {
            run_id: run_id.clone(),
        }) {
            self.record_drop(SpanDropKind::UnknownParent, key, at);
            return Vec::new();
        }
        self.start_span(StartSpec {
            key,
            parent: Some(SpanKey::Agent {
                run_id: run_id.clone(),
            }),
            name: format!("chat {model}"),
            kind: SpanKind::Client,
            at,
            attributes: vec![
                SpanAttribute::new("gen_ai.operation.name", "chat"),
                SpanAttribute::new("gen_ai.provider.name", normalize_provider(provider)),
                SpanAttribute::new("gen_ai.request.model", model.clone()),
                SpanAttribute::new("evorch.agent_run.id", run_id.clone()),
                SpanAttribute::new("evorch.request.id", request_id.clone()),
            ],
        })
    }
}

fn normalize_provider(provider: &str) -> &'static str {
    if provider.eq_ignore_ascii_case("anthropic") {
        "anthropic"
    } else if provider.eq_ignore_ascii_case("openai") {
        "openai"
    } else if provider.eq_ignore_ascii_case("openai-compatible") {
        "openai-compatible"
    } else {
        "other"
    }
}

const fn failure_name(failure: &ProviderFailureKind) -> &'static str {
    match failure {
        ProviderFailureKind::RateLimited => "rate_limited",
        ProviderFailureKind::Http { .. } => "http",
        ProviderFailureKind::Timeout => "timeout",
        ProviderFailureKind::InvalidResponse => "invalid_response",
        ProviderFailureKind::Transport => "transport",
        ProviderFailureKind::Server => "server",
        ProviderFailureKind::Quota => "quota",
        ProviderFailureKind::Auth => "auth",
        ProviderFailureKind::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_name_maps_every_variant_to_stable_snake_case() {
        // Given: every provider failure variant, including status-bearing HTTP.
        let cases = [
            (ProviderFailureKind::RateLimited, "rate_limited"),
            (ProviderFailureKind::Http { status: 503 }, "http"),
            (ProviderFailureKind::Timeout, "timeout"),
            (ProviderFailureKind::InvalidResponse, "invalid_response"),
            (ProviderFailureKind::Transport, "transport"),
            (ProviderFailureKind::Server, "server"),
            (ProviderFailureKind::Quota, "quota"),
            (ProviderFailureKind::Auth, "auth"),
            (ProviderFailureKind::Other, "other"),
        ];

        // When: each variant is classified.
        let actual = cases.map(|(failure, _)| failure_name(&failure));

        // Then: the values are stable and discard HTTP status cardinality.
        assert_eq!(
            actual,
            [
                "rate_limited",
                "http",
                "timeout",
                "invalid_response",
                "transport",
                "server",
                "quota",
                "auth",
                "other",
            ]
        );
    }
}
