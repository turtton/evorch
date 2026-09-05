//! run ID が明示された観測イベントだけを集約する telemetry overlay。
//!
//! `UsageEvent::Usage` は `run_id` を持たないため常に無視する。表示用 token 数は
//! `ProviderEvent::RequestCompleted` の値だけを使い、未知の provider/model は推測せず
//! `None` のまま保持する。

use std::collections::BTreeMap;

use event_bus::{Event, EventKind, ProviderEvent, ToolEvent};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TelemetryRow {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub current_tool: Option<String>,
    pub usage: TokenUsage,
    pub requests: u32,
    pub last_finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct TelemetryOverlay {
    rows: BTreeMap<String, TelemetryRow>,
}

impl TelemetryOverlay {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply_event(&mut self, event: &Event) {
        match &event.kind {
            EventKind::Provider(ProviderEvent::RequestStarted {
                provider,
                model,
                run_id: Some(run_id),
                ..
            }) => {
                let row = self.rows.entry(run_id.clone()).or_default();
                row.provider = Some(provider.clone());
                row.model = Some(model.clone());
                row.requests = row.requests.saturating_add(1);
            }
            EventKind::Provider(ProviderEvent::RequestCompleted {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                finish_reason,
                run_id: Some(run_id),
                ..
            }) => {
                let row = self.rows.entry(run_id.clone()).or_default();
                row.usage.input = row.usage.input.saturating_add(*input_tokens);
                row.usage.output = row.usage.output.saturating_add(*output_tokens);
                row.usage.cache_read = row.usage.cache_read.saturating_add(*cache_read_tokens);
                row.usage.cache_write = row.usage.cache_write.saturating_add(*cache_write_tokens);
                row.last_finish_reason = Some(finish_reason.clone());
            }
            EventKind::Tool(ToolEvent::ToolStarted {
                tool_name,
                run_id: Some(run_id),
                ..
            }) => {
                self.rows.entry(run_id.clone()).or_default().current_tool = Some(tool_name.clone());
            }
            EventKind::Tool(ToolEvent::ToolCompleted {
                run_id: Some(run_id),
                ..
            }) => {
                self.rows.entry(run_id.clone()).or_default().current_tool = None;
            }
            EventKind::Lifecycle(_)
            | EventKind::Message(_)
            | EventKind::Tool(_)
            | EventKind::Usage(_)
            | EventKind::Provider(_)
            | EventKind::Fault(_)
            | EventKind::AgentMessage(_)
            | EventKind::Compaction(_)
            | EventKind::Orchestrator(_) => {}
        }
    }

    pub fn row(&self, run_id: &str) -> Option<&TelemetryRow> {
        self.rows.get(run_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_bus::{Event, ProviderEvent, ToolEvent, UsageEvent};

    fn request_started(run_id: Option<&str>) -> Event {
        Event::new(ProviderEvent::RequestStarted {
            request_id: "request-1".into(),
            provider: "provider-a".into(),
            profile: None,
            protocol: "protocol-a".into(),
            model: "model-a".into(),
            streaming: true,
            run_id: run_id.map(str::to_owned),
        })
    }

    fn request_completed(run_id: Option<&str>, input: u64, output: u64) -> Event {
        Event::new(ProviderEvent::RequestCompleted {
            request_id: "request-1".into(),
            provider: "provider-a".into(),
            profile: None,
            protocol: "protocol-a".into(),
            model: "model-a".into(),
            streaming: true,
            duration_ms: 10,
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: 3,
            cache_write_tokens: 4,
            finish_reason: "stop".into(),
            run_id: run_id.map(str::to_owned),
        })
    }

    #[test]
    fn provider_and_model_come_from_request_started() {
        let mut overlay = TelemetryOverlay::new();
        overlay.apply_event(&request_started(Some("run-1")));

        let row = overlay.row("run-1").expect("telemetry row");
        assert_eq!(row.provider.as_deref(), Some("provider-a"));
        assert_eq!(row.model.as_deref(), Some("model-a"));
        assert_eq!(row.requests, 1);
    }

    #[test]
    fn tokens_accumulate_from_request_completed_only() {
        let mut overlay = TelemetryOverlay::new();
        overlay.apply_event(&request_completed(Some("run-1"), 10, 20));
        overlay.apply_event(&request_completed(Some("run-1"), 5, 7));

        assert_eq!(
            overlay.row("run-1").expect("telemetry row").usage,
            TokenUsage {
                input: 15,
                output: 27,
                cache_read: 6,
                cache_write: 8,
            }
        );
    }

    #[test]
    fn usage_event_without_run_id_is_ignored() {
        let mut overlay = TelemetryOverlay::new();
        overlay.apply_event(&Event::new(UsageEvent::Usage {
            provider: "provider-a".into(),
            model: "model-a".into(),
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 3,
            cache_write_tokens: 4,
        }));

        assert!(overlay.row("run-1").is_none());
    }

    #[test]
    fn current_tool_set_and_cleared() {
        let mut overlay = TelemetryOverlay::new();
        overlay.apply_event(&Event::new(ToolEvent::ToolStarted {
            tool_name: "read".into(),
            call_id: "call-1".into(),
            run_id: Some("run-1".into()),
        }));
        assert_eq!(
            overlay
                .row("run-1")
                .expect("telemetry row")
                .current_tool
                .as_deref(),
            Some("read")
        );

        overlay.apply_event(&Event::new(ToolEvent::ToolCompleted {
            tool_name: "read".into(),
            call_id: "call-1".into(),
            is_error: false,
            detail: None,
            run_id: Some("run-1".into()),
        }));
        assert!(
            overlay
                .row("run-1")
                .expect("telemetry row")
                .current_tool
                .is_none()
        );
    }

    #[test]
    fn missing_fields_stay_none() {
        let mut overlay = TelemetryOverlay::new();
        overlay.apply_event(&request_completed(Some("run-1"), 1, 2));
        overlay.apply_event(&request_started(None));

        let row = overlay.row("run-1").expect("telemetry row");
        assert!(row.provider.is_none());
        assert!(row.model.is_none());
        assert!(row.current_tool.is_none());
    }
}
