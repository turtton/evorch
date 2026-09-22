use super::LoopState;
use event_bus::{ContextComposition, Event, LifecycleEvent, RunActivity};
use providers::{ContentBlock, Message};

use crate::compaction::estimator::{estimate_request, estimate_tool_tokens};

fn tokens(value: &impl serde::Serialize) -> u64 {
    serde_json::to_vec(value).map_or(u64::MAX, |bytes| (bytes.len() as u64).div_ceil(4))
}

impl LoopState {
    pub(crate) fn estimated_context_tokens(&self, messages: &[Message]) -> u64 {
        estimate_request(
            messages,
            &self.tool_specs,
            self.last_usage.as_ref(),
            self.compaction.last_usage_estimated_tokens,
        )
    }

    pub(crate) fn estimated_tool_tokens(&self) -> u64 {
        estimate_tool_tokens(&self.tool_specs)
    }

    pub(crate) fn activity(&self, activity: RunActivity) {
        self.shared
            .bus
            .emit(Event::new(LifecycleEvent::RunProgress {
                run_id: self.task.run_id.to_string(),
                activity,
                context: None,
            }));
    }
    pub(super) fn publish_context(
        &self,
        messages: &[Message],
        projected_tokens: u64,
        window_tokens: u64,
    ) {
        let mut context = ContextComposition {
            tool_definitions: self.estimated_tool_tokens(),
            projected_tokens,
            window_tokens,
            ..Default::default()
        };
        for message in messages {
            if message.role == providers::Role::System {
                context.instructions = context.instructions.saturating_add(tokens(message));
            } else {
                let outputs = message
                    .content
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
                    .fold(0_u64, |sum, b| sum.saturating_add(tokens(b)));
                context.tool_outputs = context.tool_outputs.saturating_add(outputs);
                context.conversation = context
                    .conversation
                    .saturating_add(tokens(message).saturating_sub(outputs));
            }
        }
        self.shared
            .bus
            .emit(Event::new(LifecycleEvent::RunProgress {
                run_id: self.task.run_id.to_string(),
                activity: RunActivity::Model,
                context: Some(context),
            }));
    }
}
