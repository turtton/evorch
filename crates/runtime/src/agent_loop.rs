//! 単一 AgentRun のTokio実行ループ。

mod messages;
mod tool_calls;

use std::sync::{Arc, Weak};

use agents::Role;
use event_bus::{AgentRunPhase, Event, EventBus, LifecycleEvent};
use providers::{ContentBlock, FinishReason, ToolSpec};
use tokio::sync::{mpsc, watch};
use tools::ToolExecutor;

use crate::runtime::{Shared, loop_shared};
use crate::{AgentContext, AgentModel, ExecutionPolicy, RunConfig, RunId, RunMailbox, RunState};
use tool_calls::standard_tool_specs;

pub(crate) struct RunTask {
    pub(crate) run_id: RunId,
    pub(crate) role: Role,
    pub(crate) prompt: String,
    pub(crate) config: RunConfig,
    pub(crate) parent: Option<RunId>,
    pub(crate) mailbox: Arc<RunMailbox>,
}

pub(crate) struct LoopChannels {
    pub(crate) phase_tx: watch::Sender<AgentRunPhase>,
    pub(crate) message_count_tx: watch::Sender<usize>,
    pub(crate) inbox_rx: mpsc::Receiver<String>,
    pub(crate) cancel_rx: watch::Receiver<bool>,
    pub(crate) mailbox_version_rx: watch::Receiver<u64>,
}

pub(crate) struct LoopShared {
    pub(crate) bus: Arc<EventBus>,
    pub(crate) executor: Arc<ToolExecutor>,
    pub(crate) model: Arc<dyn AgentModel>,
    pub(crate) runtime: Weak<Shared>,
}

pub(crate) struct LoopState {
    task: RunTask,
    shared: LoopShared,
    channels: LoopChannels,
    run_state: RunState,
    context: AgentContext,
    policy: ExecutionPolicy,
    tool_specs: Vec<ToolSpec>,
    resumed: bool,
}

pub(crate) async fn run_agent(shared: Weak<Shared>, task: RunTask, channels: LoopChannels) {
    let Some(shared) = loop_shared(&shared) else {
        return;
    };
    let mut context = AgentContext::new(task.run_id, task.role);
    context.push_user(&task.prompt);
    let policy = ExecutionPolicy::for_role(task.role);
    let tool_specs = policy.filter_tool_specs(standard_tool_specs());
    let mut state = LoopState {
        task,
        shared,
        channels,
        run_state: RunState::new(),
        context,
        policy,
        tool_specs,
        resumed: false,
    };
    state.publish_message_count();
    if state.transition(AgentRunPhase::Running, None).is_err() {
        return;
    }
    state.execute().await;
}

impl LoopState {
    pub(crate) fn runtime(&self) -> Option<crate::AgentRuntime> {
        crate::AgentRuntime::from_weak(&self.shared.runtime)
    }

    async fn execute(&mut self) {
        loop {
            if self.cancelled() {
                self.finish_cancelled();
                return;
            }
            self.inject_parent_messages();
            let completion = tokio::select! {
                biased;
                changed = self.channels.cancel_rx.changed() => {
                    if changed.is_ok() && self.cancelled() {
                        self.finish_cancelled();
                        return;
                    }
                    continue;
                }
                result = self.shared.model.complete(
                    self.task.role,
                    &self.context.messages,
                    &self.tool_specs,
                ) => result,
            };
            let response = match completion {
                Ok(response) => response,
                Err(error) => {
                    let reason = error.to_string();
                    let _ = self.transition(AgentRunPhase::Error, Some(reason));
                    return;
                }
            };
            let finish_reason = response.finish_reason;
            let tool_uses: Vec<(String, String, serde_json::Value)> = response
                .message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    ContentBlock::Text { .. }
                    | ContentBlock::Reasoning { .. }
                    | ContentBlock::ToolResult { .. } => None,
                })
                .collect();
            let has_tool_uses = !tool_uses.is_empty();
            self.context.push_assistant(response.message);
            self.publish_message_count();
            if !self.execute_tools(tool_uses).await {
                return;
            }
            if has_tool_uses {
                continue;
            }

            match finish_reason {
                FinishReason::ToolUse => continue,
                FinishReason::Stop => {
                    if self.cancelled() {
                        self.finish_cancelled();
                        return;
                    }
                    if self.flush_aside() {
                        continue;
                    }
                    if !self.task.config.interactive || self.resumed {
                        self.finish_success();
                        return;
                    }
                    if !self.wait_for_input().await {
                        return;
                    }
                }
                FinishReason::Length => {
                    self.finish_error("model response reached length limit".to_string());
                    return;
                }
                FinishReason::ContentFilter => {
                    self.finish_error("model response was blocked by content filter".to_string());
                    return;
                }
                FinishReason::Other(reason) => {
                    self.finish_error(format!("model stopped: {reason}"));
                    return;
                }
            }
        }
    }

    async fn wait_for_input(&mut self) -> bool {
        if self.transition(AgentRunPhase::Waiting, None).is_err() {
            return false;
        }
        loop {
            tokio::select! {
                biased;
                changed = self.channels.cancel_rx.changed() => {
                    if changed.is_ok() && self.cancelled() {
                        self.finish_cancelled();
                    }
                    return false;
                }
                message = self.channels.inbox_rx.recv() => {
                    let Some(message) = message else {
                        self.finish_error("interactive inbox closed".to_string());
                        return false;
                    };
                    self.context.push_user(&message);
                    self.publish_message_count();
                    self.resumed = true;
                    return self.transition(AgentRunPhase::Running, None).is_ok();
                }
                changed = self.channels.mailbox_version_rx.changed() => {
                    if changed.is_err() {
                        continue;
                    }
                    if self.task.mailbox.is_empty() {
                        continue;
                    }
                    let messages = self.task.mailbox.drain_where(|_| true);
                    if messages.is_empty() {
                        continue;
                    }
                    self.inject_messages(messages);
                    self.resumed = true;
                    return self.transition(AgentRunPhase::Running, None).is_ok();
                }
            }
        }
    }

    pub(crate) fn transition(
        &mut self,
        phase: AgentRunPhase,
        reason: Option<String>,
    ) -> Result<(), ()> {
        let event = self
            .run_state
            .transition(self.task.run_id, phase, reason)
            .map_err(|_| ())?;
        self.shared.bus.emit(Event::new(event));
        self.channels.phase_tx.send_replace(phase);
        Ok(())
    }

    fn publish_message_count(&self) {
        self.channels
            .message_count_tx
            .send_replace(self.context.messages.len());
    }

    pub(crate) fn push_final_result(&mut self, result: &str) {
        self.context.push_assistant(providers::Message {
            role: providers::Role::Assistant,
            content: vec![ContentBlock::Text {
                text: result.to_string(),
            }],
        });
        self.publish_message_count();
    }

    fn cancelled(&self) -> bool {
        *self.channels.cancel_rx.borrow()
    }

    pub(crate) fn finish_success(&mut self) {
        if self.transition(AgentRunPhase::Done, None).is_ok() {
            self.shared
                .bus
                .emit(Event::new(LifecycleEvent::BackgroundTaskCompleted {
                    task_id: self.task.run_id.to_string(),
                }));
        }
    }

    fn finish_error(&mut self, reason: String) {
        let _ = self.transition(AgentRunPhase::Error, Some(reason));
    }

    fn finish_cancelled(&mut self) {
        self.shared
            .bus
            .emit(Event::new(LifecycleEvent::BackgroundTaskCancelled {
                task_id: self.task.run_id.to_string(),
            }));
        self.finish_error("cancelled".to_string());
    }
}
