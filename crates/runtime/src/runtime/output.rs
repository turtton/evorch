#[cfg(test)]
mod attachment_tests;
pub(super) mod idle;
#[path = "output/idle/adoption.rs"]
mod input_adoption;

use serde::Serialize;

use super::{AgentRunPhase, AgentRuntime, RunId, lock_runs, unknown_run};
use crate::RuntimeError;
use crate::output_attachments::{self, OutputKind, Snapshot, Source};

#[derive(Serialize)]
pub(crate) struct RunOutput {
    #[serde(skip)]
    pub(crate) completion_notice: Option<std::sync::Arc<crate::mailbox::CompletionNotice>>,
    run_id: String,
    phase: AgentRunPhase,
    status: OutputStatus,
    output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_generation: Option<u64>,
    reason: Option<String>,
    /// For a finished direct child: whether a message continues it with its
    /// saved context, which is preferable to delegating the same work again.
    #[serde(skip_serializing_if = "Option::is_none")]
    continuable: Option<bool>,
    /// Why a finished direct child cannot be continued.
    #[serde(skip_serializing_if = "Option::is_none")]
    continuation_blocked: Option<String>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum OutputStatus {
    StillRunning,
    Waiting,
    TurnCompleted,
    Completed,
    Cancelled,
    Stopped,
    Failed,
}

/// A live interactive turn is distinct from a terminal run incarnation.
pub(super) struct IdleTurn {
    output: Option<String>,
    pub(super) attachable: bool,
    pub(in crate::runtime) notice: std::sync::Arc<crate::mailbox::CompletionNotice>,
}

impl AgentRuntime {
    pub(crate) fn observe_queued_message(&self, queued: &crate::mailbox::QueuedMessage) {
        use crate::mailbox::completion::MessageOrigin;
        match &queued.origin {
            MessageOrigin::Explicit => {}
            MessageOrigin::CompletionNotice(notice) => self.observe_completion(notice),
            MessageOrigin::QuestionNotice(notice) => self.observe_questions([notice.id.clone()]),
        }
    }

    /// Publish a nonterminal phase and its optional idle output atomically.
    /// Only successful interactive turn boundaries supply a completed turn;
    /// question waits, nested waits and escalation waits must not synthesize one.
    pub(crate) fn publish_live_phase(
        &self,
        run: RunId,
        event: event_bus::LifecycleEvent,
        completed_turn: Option<Option<String>>,
        phase_tx: &tokio::sync::watch::Sender<AgentRunPhase>,
        release_adopted: impl FnOnce(),
    ) {
        let event_bus::LifecycleEvent::AgentRunStateChanged { to, .. } = &event else {
            return;
        };
        let mut runs = lock_runs(&self.shared.runs);
        // Releasing this turn's adopted inputs and publishing its replacement
        // boundary are one registry operation. New sends use this same lock.
        phase_tx.send_replace(*to);
        release_adopted();
        if let Some(entry) = runs.get_mut(&run) {
            let input = entry.user_inbox.status();
            let attachable = input.pending == 0
                && !input.next_turn_requested
                && entry.inbox_tx.capacity() == entry.inbox_tx.max_capacity()
                && !entry.mailbox.has_interrupting_message()
                // Read after the queues: a dequeue must first hold adoption.
                && !entry.user_inbox.adopting();
            entry.idle_turn = completed_turn.map(|output| IdleTurn {
                output,
                attachable,
                notice: crate::mailbox::CompletionNotice::new(
                    entry.parent,
                    run,
                    self.shared
                        .next_completion_generation
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                ),
            });
            if entry.idle_turn.is_some() {
                self.shared
                    .turn_version
                    .send_modify(|version| *version += 1);
            }
        }
        let became_running = *to == AgentRunPhase::Running;
        self.shared
            .turn_version
            .send_modify(|v| *v = v.wrapping_add(1));
        self.shared.bus.emit(event_bus::Event::new(event));
        drop(runs);
        if became_running {
            self.schedule_question_recovery(run);
        }
    }

    /// Only output actually returned by a model-facing tool acknowledges a receipt.
    /// The snapshot's receipt, not today's registry entry, fixes the generation.
    pub(crate) fn observe_completion(&self, notice: &crate::mailbox::CompletionNotice) {
        let Some(recipient) = notice.recipient else {
            return;
        };
        let runs = lock_runs(&self.shared.runs);
        tracing::trace!(%recipient, child = %notice.child, generation = notice.generation, "child output observed");
        notice.observe();
        if let Some(entry) = runs.get(&recipient) {
            entry.mailbox.discard_observed_completions();
        }
        let mut held = self
            .shared
            .held_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(queue) = held.get_mut(&recipient) {
            queue.retain(|message| !message.observed());
        }
    }

    /// Live phase and terminal reason of a registered run, with no relation check.
    /// Only for runs whose conversation the caller is already authorized to read
    /// (ADR 0041 thread tools); `run_output` keeps its parent/child rule.
    pub(crate) fn live_run_status(&self, run: RunId) -> Option<(AgentRunPhase, Option<String>)> {
        let runs = lock_runs(&self.shared.runs);
        let entry = runs.get(&run)?;
        Some((*entry.phase_rx.borrow(), entry.terminal_reason.clone()))
    }

    pub(crate) fn run_output(
        &self,
        caller: RunId,
        target: RunId,
    ) -> Result<RunOutput, RuntimeError> {
        let admission = self.admission_snapshot(target);
        let runs = lock_runs(&self.shared.runs);
        let caller_entry = runs.get(&caller).ok_or_else(|| unknown_run(caller))?;
        let Some(entry) = runs.get(&target) else {
            if caller != target
                && let Some(admission) = admission
                && admission.parent == Some(caller)
            {
                let (phase, status, reason) = match admission.result {
                    None => (AgentRunPhase::Pending, OutputStatus::StillRunning, None),
                    Some(Err(RuntimeError::RunTerminated { .. })) => (
                        AgentRunPhase::Error,
                        OutputStatus::Cancelled,
                        Some("cancelled".into()),
                    ),
                    Some(Err(error)) => (
                        AgentRunPhase::Error,
                        OutputStatus::Failed,
                        Some(error.to_string()),
                    ),
                    Some(Ok(())) => return Err(unknown_run(target)),
                };
                return Ok(RunOutput {
                    completion_notice: None,
                    run_id: target.to_string(),
                    phase,
                    status,
                    output: None,
                    output_ref: None,
                    output_bytes: None,
                    turn_generation: None,
                    reason,
                    continuable: None,
                    continuation_blocked: None,
                });
            }
            return Err(unknown_run(target));
        };
        if caller == target || (caller_entry.parent != Some(target) && entry.parent != Some(caller))
        {
            return Err(RuntimeError::MessageDenied {
                sender: caller,
                recipient: target,
                detail: "run_output requires distinct directly related parent/child runs".into(),
            });
        }
        // Terminal publication holds this same lock, so phase, reason and output agree.
        let (phase, status, output) = with_output(entry, |phase, status, output| {
            (phase, status, output.map(str::to_owned))
        });
        let reason = matches!(phase, AgentRunPhase::Stopped | AgentRunPhase::Error)
            .then(|| entry.terminal_reason.clone())
            .flatten();
        let kind = (entry.parent == Some(caller))
            .then(|| attachment_kind(entry, status))
            .flatten();
        let finished_child = entry.parent == Some(caller)
            && matches!(
                phase,
                AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
            );
        let turn_generation = entry
            .idle_turn
            .as_ref()
            .filter(|_| phase == AgentRunPhase::Waiting)
            .map(|turn| turn.notice.generation);
        let completion_notice = if finished_child {
            Some(entry.completion_notice.clone())
        } else if entry.parent == Some(caller) && phase == AgentRunPhase::Waiting {
            entry.idle_turn.as_ref().map(|turn| turn.notice.clone())
        } else {
            None
        };
        // Saved state is read without holding the runs lock.
        drop(runs);
        let metadata = kind
            .zip(output.as_deref())
            .filter(|(_, output)| !output.trim().is_empty())
            .map(|(kind, output)| {
                (
                    output_attachments::output_ref(target, kind, output),
                    output.len(),
                )
            });
        let availability = finished_child.then(|| self.continuation_availability(caller, target));
        Ok(RunOutput {
            completion_notice,
            run_id: target.to_string(),
            phase,
            status,
            output,
            output_ref: metadata.as_ref().map(|m| m.0.clone()),
            output_bytes: metadata.map(|m| m.1),
            turn_generation,
            reason,
            continuable: availability.as_ref().map(Result::is_ok),
            continuation_blocked: availability.and_then(Result::err),
        })
    }
}

/// Borrow the canonical output under the registry lock. Consumers decide whether
/// to copy; the attachment path checks raw byte limits before any body allocation.
fn with_output<T>(
    entry: &super::RunEntry,
    f: impl FnOnce(AgentRunPhase, OutputStatus, Option<&str>) -> T,
) -> T {
    let phase = *entry.phase_rx.borrow();
    match phase {
        AgentRunPhase::Done => f(
            phase,
            OutputStatus::Completed,
            entry.result_rx.borrow().as_deref(),
        ),
        AgentRunPhase::Waiting => match &entry.idle_turn {
            Some(turn) => f(phase, OutputStatus::TurnCompleted, turn.output.as_deref()),
            None => f(phase, OutputStatus::Waiting, None),
        },
        AgentRunPhase::Stopped => f(phase, OutputStatus::Stopped, None),
        AgentRunPhase::Error => f(
            phase,
            if entry.terminal_reason.as_deref() == Some("cancelled") {
                OutputStatus::Cancelled
            } else {
                OutputStatus::Failed
            },
            None,
        ),
        AgentRunPhase::Pending | AgentRunPhase::Running => {
            f(phase, OutputStatus::StillRunning, None)
        }
    }
}

fn attachment_kind(entry: &super::RunEntry, status: OutputStatus) -> Option<OutputKind> {
    match status {
        OutputStatus::Completed => Some(OutputKind::Completed),
        // New input invalidates the old turn before Running is published. Retain
        // run_output's existing output/ACK semantics, but offer no stale reference.
        OutputStatus::TurnCompleted
            if entry.idle_turn.as_ref().is_some_and(|turn| turn.attachable) =>
        {
            Some(OutputKind::TurnCompleted)
        }
        _ => None,
    }
}

impl AgentRuntime {
    /// No storage/continuation enrichment, restoration, wake, ACK or progress.
    pub(crate) fn output_attachment_snapshots(
        &self,
        caller: RunId,
        sources: &[Source],
    ) -> Result<Vec<Snapshot>, String> {
        // Registration's lock order is admissions -> runs, never the reverse.
        let admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let runs = lock_runs(&self.shared.runs);
        // Check every relation before inspecting phase or content; denied sources
        // (including nonexistent, self, parent and non-live children) look alike.
        for (index, source) in sources.iter().enumerate() {
            let direct = caller != source.run
                && runs.contains_key(&caller)
                && runs.get(&source.run).map_or_else(
                    || {
                        admissions.get(&source.run).is_some_and(|a| {
                            let snapshot = a.snapshot();
                            snapshot.parent == Some(caller) && snapshot.result.is_none()
                        })
                    },
                    |e| e.parent == Some(caller),
                );
            if !direct {
                return Err(output_attachments::source_error(
                    "attachment_source_denied",
                    index,
                ));
            }
        }
        let mut total = 0;
        let mut kinds = Vec::with_capacity(sources.len());
        // First pass validates all sizes before copying ANY bodies.
        for (index, source) in sources.iter().enumerate() {
            let Some(entry) = runs.get(&source.run) else {
                return Err(output_attachments::source_error(
                    "attachment_source_not_ready",
                    index,
                ));
            };
            let (kind, bytes) = with_output(entry, |_, status, output| {
                if matches!(
                    status,
                    OutputStatus::Stopped | OutputStatus::Cancelled | OutputStatus::Failed
                ) {
                    return Err(output_attachments::source_error(
                        "attachment_source_unsuccessful",
                        index,
                    ));
                }
                let kind = attachment_kind(entry, status).ok_or_else(|| {
                    output_attachments::source_error("attachment_source_not_ready", index)
                })?;
                let output = output
                    .filter(|output| !output.trim().is_empty())
                    .ok_or_else(|| {
                        output_attachments::source_error("attachment_output_missing", index)
                    })?;
                if output.len() > output_attachments::MAX_OUTPUT_BYTES {
                    return Err(output_attachments::too_large(
                        Some(index),
                        "output",
                        output.len(),
                        output_attachments::MAX_OUTPUT_BYTES,
                    ));
                }
                Ok((kind, output.len()))
            })?;
            kinds.push(kind);
            total += bytes;
            if total > output_attachments::MAX_TOTAL_BYTES {
                return Err(output_attachments::too_large(
                    Some(index),
                    "total_output",
                    total,
                    output_attachments::MAX_TOTAL_BYTES,
                ));
            }
        }
        Ok(sources
            .iter()
            .zip(kinds)
            .map(|(source, kind)| {
                let entry = &runs[&source.run];
                with_output(entry, |_, _, output| Snapshot {
                    run: source.run,
                    kind,
                    output: output.expect("validated under same lock").into(),
                })
            })
            .collect())
    }
}
