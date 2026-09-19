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
        // team_task) を持たない run は goal ledger の所有物にならず、境界イベントを
        // 永続化すると起動時の goal replay が未解決イベントで失敗するため emit しない。
        // chat 履歴復元は run_contexts スナップショット経由で、この経路には依存しない。
        if self.task.config.task_id.is_none() && self.task.config.team_task.is_none() {
            return;
        }
        let status = match phase {
            AgentRunPhase::Pending => TaskStatus::Pending,
            AgentRunPhase::Running | AgentRunPhase::Waiting => TaskStatus::Running,
            AgentRunPhase::Done => TaskStatus::Completed,
            AgentRunPhase::Error => match reason.as_deref() {
                Some("cancelled") => TaskStatus::Cancelled,
                Some(_) | None => TaskStatus::Failed,
            },
        };
        let task = self.durable_task.get_or_insert_with(|| {
            self.task
                .config
                .task_id
                .as_ref()
                .and_then(|_| {
                    self.task
                        .prompt
                        .lines()
                        .find_map(|line| serde_json::from_str::<TaskContinuation>(line).ok())
                })
                .unwrap_or_else(|| TaskContinuation {
                    status,
                    input: Some(self.task.prompt.clone()),
                    resume_cursor: None,
                    last_artifact: None,
                    failure_reason: None,
                    attempts: 0,
                    heartbeat_at_ns: None,
                })
        });
        let has_output = self
            .context
            .messages
            .iter()
            .any(|message| message.role == providers::Role::Assistant);
        if has_output
            || !matches!(
                (task.status, status),
                (TaskStatus::Retrying, TaskStatus::Running)
            )
        {
            task.status = status;
        }
        if let Some(reason) = reason {
            task.failure_reason = Some(reason);
        }
        // Only publish a new cursor after this generation produced an assistant message.
        // A fresh resumed generation must retain its inherited cursor while awaiting the model.
        if has_output {
            if let Ok(cursor) = serde_json::to_string(&self.context.visible_messages()) {
                task.resume_cursor = Some(cursor);
            }
            if !matches!(status, TaskStatus::Failed | TaskStatus::Cancelled)
                && let Some(artifact) = self
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
                                ContentBlock::Image { .. }
                                | ContentBlock::Reasoning { .. }
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
        if let Ok(progress) = serde_json::to_value(task) {
            self.shared
                .bus
                .emit(Event::new(OrchestratorEvent::TaskProgressed {
                    task_id: task_id.into(),
                    run_id,
                    progress,
                    reason: "task execution boundary".into(),
                }));
        }
    }
}
