use event_bus::{AgentRunPhase, Event, OrchestratorEvent};
use providers::ContentBlock;
use storage::entity::{TaskContinuation, TaskStatus};

use super::LoopState;

impl LoopState {
    pub(super) fn publish_durable_task(&mut self, phase: AgentRunPhase, reason: Option<String>) {
        if self.task.role != crate::Role::Worker {
            return;
        }
        // chat や headless の単発実行のような durable task 識別子 (task_id /
        // team_task) を持たない run は durable task ではないため emit しない。
        // chat 履歴復元は run_contexts スナップショット経由で、この経路には依存しない。
        if self.task.config.task_id.is_none() && self.task.config.team_task.is_none() {
            return;
        }
        let status = match phase {
            AgentRunPhase::Pending => TaskStatus::Pending,
            AgentRunPhase::Running | AgentRunPhase::Waiting => TaskStatus::Running,
            AgentRunPhase::Done => TaskStatus::Completed,
            AgentRunPhase::Stopped => TaskStatus::Stopped,
            AgentRunPhase::Error => match reason.as_deref() {
                Some("cancelled") => TaskStatus::Cancelled,
                Some(_) | None => TaskStatus::Failed,
            },
        };
        self.durable_task.get_or_insert_with(|| TaskContinuation {
            status,
            input: Some(self.task.prompt.clone()),
            last_artifact: None,
            failure_reason: None,
            attempts: 0,
            heartbeat_at_ns: None,
        });
        if !matches!(status, TaskStatus::Failed | TaskStatus::Cancelled) {
            self.update_durable_artifact();
        }
        let task = self.durable_task.as_mut().expect("initialized above");
        task.status = status;
        if phase == AgentRunPhase::Stopped {
            task.failure_reason = Some("stopped".into());
        } else if let Some(reason) = reason {
            task.failure_reason = Some(reason);
        }
        task.heartbeat_at_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| u64::try_from(elapsed.as_nanos()).ok());
        let run_id = self.task.run_id.to_string();
        let task_id = self
            .task
            .config
            .task_id
            .as_deref()
            .or_else(|| {
                self.task
                    .config
                    .team_task
                    .as_ref()
                    .map(|task| task.id.as_str())
            })
            .unwrap_or(&run_id);
        if let Ok(progress) = serde_json::to_value(&*task) {
            self.shared
                .bus
                .emit(Event::new(OrchestratorEvent::TaskProgressed {
                    task_id: task_id.into(),
                    run_id,
                    progress,
                    reason: "task execution boundary".into(),
                }));
            // Input is immutable task metadata, not a repeated execution snapshot.
            // The storage projection and GUI retain the first published value.
            task.input = None;
        }
    }

    /// Keep the last valid artifact without serializing or publishing conversation state.
    /// Budget boundaries still call this before a later response can fail or be truncated.
    pub(super) fn update_durable_artifact(&mut self) {
        let Some(task) = self.durable_task.as_mut() else {
            return;
        };
        if let Some(artifact) = self
            .context
            .messages
            .iter()
            .rev()
            .filter(|message| message.role == providers::Role::Assistant)
            .find_map(|message| {
                let text = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        ContentBlock::Compaction { .. } => {
                            tracing::warn!("この処理では compaction block をスキップします");
                            None
                        }
                        ContentBlock::Image { .. }
                        | ContentBlock::Reasoning { .. }
                        | ContentBlock::Anthropic { .. }
                        | ContentBlock::ResponsesReasoning { .. }
                        | ContentBlock::ToolUse { .. }
                        | ContentBlock::ToolResult { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                (!text.is_empty()).then_some(text)
            })
        {
            task.last_artifact = Some(artifact);
        }
    }
}
