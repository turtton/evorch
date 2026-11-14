//! Batch-aware wait checkpointing. No external operation is replayed here.
#[cfg(test)]
mod tests;
use super::LoopState;
use crate::quiesce::{
    DurableWait, DurableWaitMode, ObservedGeneration, WaitContinuation, WaitKind, WaitPayload,
};
use providers::ContentBlock;
use std::collections::HashSet;

pub(crate) fn unix_ms() -> Result<u64, String> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis()
        .try_into()
        .map_err(|_| "wait timestamp overflow".into())
}

impl LoopState {
    fn pending_waits(&self, reply: Option<(&str, u64)>) -> Result<Vec<DurableWait>, String> {
        let runtime = self.runtime().ok_or("runtime disappeared")?;
        let results: HashSet<_> = self
            .context
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
                _ => None,
            })
            .collect();
        let mut waits = Vec::new();
        let mut position = 0;
        for block in self.context.messages.iter().flat_map(|m| &m.content) {
            let ContentBlock::ToolUse { id, name, input } = block else {
                continue;
            };
            let index = position;
            position += 1;
            if results.contains(id.as_str()) {
                continue;
            }
            // A restored wait already carries its original deadline and projection.
            if let Some(wait) = self.recovery_waits.iter().find(|w| &w.tool_call_id == id) {
                waits.push(wait.clone());
                continue;
            }
            let (targets, mode, legacy_result, consume_mailbox, kind) = match name.as_str() {
                "wait" => {
                    let (targets, mode, legacy) = crate::restore::waiting::wait_targets(input)?;
                    (targets, mode, legacy, true, WaitKind::Runs)
                }
                "delegate"
                    if self
                        .delegate_wait_calls
                        .iter()
                        .any(|c| &c.tool_call_id == id) =>
                {
                    let calls = self.delegate_wait_calls.clone();
                    (
                        calls.iter().map(|c| c.child).collect(),
                        DurableWaitMode::All,
                        false,
                        false,
                        WaitKind::Delegates { calls },
                    )
                }
                "wait_reply" => {
                    let message_id = input
                        .get("message_id")
                        .and_then(|v| v.as_str())
                        .ok_or("missing message_id")?;
                    let deadline_unix_ms = reply
                        .filter(|(call, _)| *call == id)
                        .map(|(_, deadline)| deadline)
                        .ok_or("unstarted reply wait cannot be checkpointed")?;
                    let target = runtime
                        .reply_wait_target(self.task.run_id, message_id)
                        .map_err(|e| e.to_string())?;
                    let mut delivered_answer_ids: Vec<_> =
                        self.delivered_answer_ids().into_iter().collect();
                    delivered_answer_ids.sort();
                    (
                        vec![target],
                        DurableWaitMode::Any,
                        false,
                        true,
                        WaitKind::Reply {
                            message_id: message_id.into(),
                            deadline_unix_ms,
                            delivered_answer_ids,
                        },
                    )
                }
                _ => {
                    return Err(
                        "unfinished batch contains an unconfirmed or unexecuted non-wait tool"
                            .into(),
                    );
                }
            };
            let mut completions = Vec::new();
            for target in &targets {
                let output = runtime
                    .run_output(self.task.run_id, *target)
                    .map_err(|e| e.to_string())?;
                if let Some(notice) = output.completion_notice
                    && notice.observed()
                {
                    completions.push(ObservedGeneration {
                        id: target.to_string(),
                        generation: notice.generation,
                    });
                }
            }
            let question_generation = *runtime.shared.question_version.borrow();
            let mut questions: Vec<_> = self
                .reported_questions
                .iter()
                .map(|id| ObservedGeneration {
                    id: id.clone(),
                    generation: question_generation,
                })
                .collect();
            questions.sort_by(|a, b| a.id.cmp(&b.id));
            let mut answers: Vec<_> = self
                .delivered_answer_ids()
                .into_iter()
                .map(|id| ObservedGeneration {
                    id,
                    generation: question_generation,
                })
                .collect();
            answers.sort_by(|a, b| a.id.cmp(&b.id));
            waits.push(DurableWait {
                requester: self.task.run_id,
                targets,
                mode,
                tool_call_id: id.clone(),
                completions,
                questions,
                answers,
                continuation: WaitContinuation {
                    tool_name: name.clone(),
                    legacy_result,
                    consume_mailbox,
                    payload: Some(WaitPayload {
                        position: index,
                        input: input.clone(),
                        kind,
                    }),
                },
            });
        }
        crate::restore::waiting::validate_wait_messages(
            self.task.run_id,
            &self.context.messages,
            &waits,
        )?;
        Ok(waits)
    }

    /// Called only while an observation future is alive. A failed proof parks
    /// fail-closed; it never returns a successful safety acknowledgement.
    pub(crate) async fn quiesce_tools_waiting(&mut self, reply: Option<(&str, u64)>) {
        let Some(runtime) = self.runtime() else {
            return;
        };
        if runtime.shared.quiesce.running() {
            return;
        }
        match self.pending_waits(reply) {
            Ok(waits) => self.recovery_waits = waits,
            Err(_) => {
                // Keep the original observation future live. An unfinished batch
                // may resolve normally, but is not evidence for a restart ACK.
                runtime.shared.quiesce.block(
                    self.task.run_id,
                    &self.task.publication.incarnation(),
                    crate::quiesce::RestartBlockerKind::UnfinishedToolBatch,
                );
                return;
            }
        }
        self.quiesce_safe_point(crate::checkpoint::CheckpointBoundary::ToolsWaiting)
            .await;
    }

    pub(super) async fn resume_recovery_waits(&mut self) -> bool {
        while let Some(wait) = self.recovery_waits.first().cloned() {
            let Some(runtime) = self.runtime() else {
                self.finish_error("runtime disappeared".into());
                return false;
            };
            self.reported_questions
                .extend(wait.questions.iter().map(|g| g.id.clone()));
            self.meta_call_id = Some(wait.tool_call_id.clone());
            let results = Box::pin(crate::meta::resume_wait(self, &runtime, &wait)).await;
            self.meta_call_id = None;
            for (id, result) in results {
                self.ledger_tool_completed(&wait.continuation.tool_name, &id, &result);
                self.context.push_tool_result(id.clone(), result);
                self.recovery_waits.retain(|w| w.tool_call_id != id);
                self.publish_message_count();
            }
            if let Some(kind) = self.interrupted() {
                self.finish_interrupted(kind);
                return false;
            }
        }
        true
    }
}
