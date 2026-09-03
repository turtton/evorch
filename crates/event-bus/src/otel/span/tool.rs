use std::time::SystemTime;

use crate::event::ToolEvent;

use super::state::{EndSpec, StartSpec, terminal_error};
use super::{SpanAction, SpanAttribute, SpanDropKind, SpanKey, SpanKind, SpanMapper, SpanStatus};

impl SpanMapper {
    pub(super) fn map_tool(&mut self, event: &ToolEvent, at: SystemTime) -> Vec<SpanAction> {
        match event {
            ToolEvent::ToolStarted { .. } => self.start_tool(event, at),
            ToolEvent::ToolCompleted {
                call_id, is_error, ..
            } => {
                let status = if *is_error {
                    SpanStatus::Error
                } else {
                    SpanStatus::Unset
                };
                self.end_span(EndSpec {
                    key: SpanKey::Tool {
                        call_id: call_id.clone(),
                    },
                    at,
                    status,
                    terminal: terminal_error(is_error.then_some("tool_error")),
                })
            }
            ToolEvent::ApprovalRequested { .. }
            | ToolEvent::ApprovalResolved { .. }
            | ToolEvent::ExecutionDenied { .. } => Vec::new(),
        }
    }

    fn start_tool(&mut self, event: &ToolEvent, at: SystemTime) -> Vec<SpanAction> {
        let ToolEvent::ToolStarted {
            tool_name,
            call_id,
            run_id,
        } = event
        else {
            return Vec::new();
        };
        let key = SpanKey::Tool {
            call_id: call_id.clone(),
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
            name: format!("execute_tool {tool_name}"),
            kind: SpanKind::Internal,
            at,
            attributes: vec![
                SpanAttribute::new("gen_ai.operation.name", "execute_tool"),
                SpanAttribute::new("gen_ai.provider.name", "evorch"),
                SpanAttribute::new("gen_ai.tool.name", tool_name.clone()),
                SpanAttribute::new("gen_ai.tool.call.id", call_id.clone()),
                SpanAttribute::new("evorch.agent_run.id", run_id.clone()),
            ],
        })
    }
}
