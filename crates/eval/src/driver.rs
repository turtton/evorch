//! Drive one replay: start the recorded first request, let the simulated user
//! answer and follow up at turn boundaries, and stop at the user's finish, the
//! root run's own end, or a budget (ADR 0036 §5, §6).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use config::EvalBudgetConfig;
use event_bus::{
    AgentRunPhase, DiagnosticSeverity, Event, EventBus, EventKind, LifecycleEvent, MessageEvent,
    ProviderEvent, RequestPurpose, ToolEvent, event::diagnostic_codes,
};
use runtime::{AgentRuntime, RunConfig, StopScope};
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, watch};

use crate::conversation::Entry;
use crate::simulator::{Action, PendingQuestion, UserSimulator};
use crate::source::OriginalThread;

const SETTLE_TIMEOUT: Duration = Duration::from_secs(60);
const INPUT_SUMMARY_CHARS: usize = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Completed,
    Incomplete,
    Failed,
    PrepareFailed,
}

/// A request the replay refused because it would leave the evaluation environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeniedRequest {
    pub tool: String,
    pub input: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplayOutcome {
    pub status: Status,
    pub reason: String,
    pub user_turns: u32,
    pub tokens: u64,
    pub tool_calls: u32,
    pub elapsed_ms: u64,
    pub denied_requests: Vec<DeniedRequest>,
    #[serde(skip)]
    pub conversation: Vec<Entry>,
}

pub struct Replay<'a> {
    pub runtime: &'a AgentRuntime,
    pub bus: &'a Arc<EventBus>,
    pub thread: &'a OriginalThread,
    pub simulator: &'a UserSimulator,
    pub budget: &'a EvalBudgetConfig,
    /// Authority for the root run and every follow-up sent to it.
    pub chat: RunConfig,
}

#[derive(Default)]
struct Observed {
    root: Option<String>,
    root_phase: Option<AgentRunPhase>,
    root_reason: Option<String>,
    turns_completed: u64,
    conversation: Vec<Entry>,
    pending: Vec<PendingQuestion>,
    tokens: u64,
    tool_calls: u32,
    denied: Vec<DeniedRequest>,
    tool_inputs: HashMap<String, (String, String)>,
    /// Events the observer skipped because it fell behind the bus.
    lagged_events: u64,
}

impl Observed {
    fn is_root(&self, run_id: &str) -> bool {
        self.root.as_deref() == Some(run_id)
    }

    fn observe(&mut self, event: &Event, bus: &EventBus) {
        match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStarted {
                run_id,
                parent_run_id: None,
                ..
            }) if self.root.is_none() => self.root = Some(run_id.clone()),
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to,
                reason,
                ..
            }) if self.is_root(run_id) => {
                self.root_phase = Some(*to);
                self.root_reason.clone_from(reason);
            }
            EventKind::Lifecycle(LifecycleEvent::TurnCompleted { run_id, .. })
                if self.is_root(run_id) =>
            {
                self.turns_completed += 1;
            }
            EventKind::Message(MessageEvent::MessageCompleted { run_id, text })
                if self.is_root(run_id) && !text.trim().is_empty() =>
            {
                self.conversation
                    .push(Entry::Assistant { text: text.clone() });
            }
            // The root's final report is published by `finish`, not as a model
            // response, so it reaches the user only through this event. A report
            // that repeats the response just shown is not shown twice.
            EventKind::Message(MessageEvent::FinalResultPublished { run_id, text })
                if self.is_root(run_id)
                    && !text.trim().is_empty()
                    && !matches!(
                        self.conversation.last(),
                        Some(Entry::Assistant { text: last }) if last == text
                    ) =>
            {
                self.conversation
                    .push(Entry::Assistant { text: text.clone() });
            }
            EventKind::Tool(ToolEvent::UserQuestionUpdated { question })
                if question.is_user_visible() && self.is_root(&question.root_run_id) =>
            {
                let known = self
                    .pending
                    .iter()
                    .position(|pending| pending.id == question.id);
                match (&question.answer, known) {
                    (None, None) => {
                        self.conversation.push(Entry::Question {
                            id: question.id.clone(),
                            title: question.title.clone(),
                            options: question.options.clone(),
                        });
                        self.pending.push(PendingQuestion {
                            id: question.id.clone(),
                            title: question.title.clone(),
                            options: question.options.clone(),
                        });
                    }
                    (Some(_), Some(index)) => {
                        self.pending.remove(index);
                    }
                    _ => {}
                }
            }
            EventKind::Tool(ToolEvent::ToolStarted {
                tool_name,
                call_id,
                input,
                ..
            }) => {
                self.tool_calls = self.tool_calls.saturating_add(1);
                self.tool_inputs.insert(
                    call_id.clone(),
                    (tool_name.clone(), summary(input.as_ref())),
                );
            }
            EventKind::Tool(ToolEvent::ApprovalRequested {
                tool_name,
                call_id,
                input,
            }) => {
                bus.emit(Event::new(ToolEvent::ApprovalResolved {
                    call_id: call_id.clone(),
                    approved: false,
                }));
                self.denied.push(DeniedRequest {
                    tool: tool_name.clone(),
                    input: summary(input.as_ref()),
                    reason: "approval requests are denied in evaluation replays".into(),
                });
            }
            EventKind::Diagnostic(diagnostic)
                if diagnostic.source == "sandbox"
                    && diagnostic.code == diagnostic_codes::ESCALATION_REVIEW
                    && diagnostic.severity == DiagnosticSeverity::Warning =>
            {
                let (tool, input) = diagnostic
                    .call_id
                    .as_ref()
                    .and_then(|call| self.tool_inputs.get(call))
                    .cloned()
                    .unwrap_or_default();
                self.denied.push(DeniedRequest {
                    tool,
                    input,
                    reason: diagnostic.detail.clone(),
                });
            }
            EventKind::Provider(ProviderEvent::RequestCompleted {
                input_tokens,
                output_tokens,
                purpose,
                ..
            }) if *purpose != Some(RequestPurpose::UserSimulation) => {
                self.tokens = self
                    .tokens
                    .saturating_add(*input_tokens)
                    .saturating_add(*output_tokens);
            }
            _ => {}
        }
    }

    fn budget_breach(&self, budget: &EvalBudgetConfig) -> Option<String> {
        if self.tokens > budget.max_tokens {
            return Some(format!(
                "token budget exhausted ({} > {})",
                self.tokens, budget.max_tokens
            ));
        }
        (self.tool_calls > budget.max_tool_calls).then(|| {
            format!(
                "tool call budget exhausted ({} > {})",
                self.tool_calls, budget.max_tool_calls
            )
        })
    }
}

fn summary(input: Option<&serde_json::Value>) -> String {
    let text = input.map(ToString::to_string).unwrap_or_default();
    match text.char_indices().nth(INPUT_SUMMARY_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text,
    }
}

struct Observer {
    state: Arc<Mutex<Observed>>,
    changed: watch::Receiver<u64>,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Observer {
    fn start(bus: &Arc<EventBus>) -> Self {
        let state = Arc::new(Mutex::new(Observed::default()));
        let (notify, changed) = watch::channel(0_u64);
        let (stop, mut stopped) = oneshot::channel();
        let mut receiver = bus.subscribe();
        let bus = Arc::clone(bus);
        let observed = Arc::clone(&state);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    event = receiver.recv() => match event {
                        Ok(event) => {
                            lock(&observed).observe(&event, &bus);
                            notify.send_modify(|version| *version = version.wrapping_add(1));
                        }
                        Err(event_bus::RecvError::Lagged(skipped)) => {
                            tracing::warn!(skipped, "evaluation replay observer lagged");
                            let mut state = lock(&observed);
                            state.lagged_events = state.lagged_events.saturating_add(skipped);
                        }
                        Err(event_bus::RecvError::Closed) => break,
                    },
                    _ = &mut stopped => {
                        let (pending, skipped) = receiver.drain_pending_snapshot_counting_lag();
                        let mut state = lock(&observed);
                        state.lagged_events = state.lagged_events.saturating_add(skipped);
                        for event in pending {
                            state.observe(&event, &bus);
                        }
                        break;
                    }
                }
            }
        });
        Self {
            state,
            changed,
            stop: Some(stop),
            task,
        }
    }

    async fn finish(mut self) -> Observed {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = (&mut self.task).await;
        std::mem::take(&mut *lock(&self.state))
    }
}

fn lock(state: &Mutex<Observed>) -> std::sync::MutexGuard<'_, Observed> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Run the replay to its end and report what happened. Never fails: every
/// problem becomes a `failed` outcome with its reason.
pub async fn drive(replay: Replay<'_>) -> ReplayOutcome {
    let started = Instant::now();
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(replay.budget.max_elapsed_secs);
    let mut observer = Observer::start(replay.bus);
    let initial = replay.thread.initial_prompt().to_owned();
    lock(&observer.state).conversation.push(Entry::User {
        text: initial.clone(),
    });
    let root = replay.runtime.delegate_chat(
        &replay.thread.thread_id,
        replay.thread.role,
        initial,
        replay.chat.clone(),
    );
    let mut user_turns = 1_u32;
    let (status, reason) = match root {
        Err(error) => (
            Status::Failed,
            format!("the replay could not start: {error}"),
        ),
        Ok(root) => {
            let ended = converse(&replay, root, &mut observer, &mut user_turns, deadline).await;
            let stopped = match ended.0 {
                Status::Incomplete => replay.runtime.cancel_subtree(root),
                _ => replay.runtime.stop(root, StopScope::Subtree),
            };
            if let Err(error) = stopped {
                tracing::debug!(%error, "replay root was already gone");
            }
            settle(replay.runtime).await;
            ended
        }
    };
    let observed = observer.finish().await;
    let reason = match missing_report(status, &observed) {
        Some(note) => {
            tracing::warn!(%note, "evaluation replay conversation is incomplete");
            format!("{reason}; {note}")
        }
        None => reason,
    };
    ReplayOutcome {
        status,
        reason,
        user_turns,
        tokens: observed.tokens,
        tool_calls: observed.tool_calls,
        elapsed_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        denied_requests: observed.denied,
        conversation: observed.conversation,
    }
}

/// A note for a completed replay whose conversation ends with the user's input,
/// without the root's answer to it, so the lost final report is visible in the
/// outcome instead of being mistaken for a replay that never produced one.
fn missing_report(status: Status, observed: &Observed) -> Option<String> {
    if status != Status::Completed
        || !matches!(
            observed.conversation.last(),
            Some(Entry::User { .. } | Entry::Answer { .. })
        )
    {
        return None;
    }
    let mut note = "the root run's final report is missing from the conversation".to_owned();
    if observed.lagged_events > 0 {
        note.push_str(&format!(
            " (the observer skipped {} events after falling behind the event bus)",
            observed.lagged_events
        ));
    }
    Some(note)
}

async fn converse(
    replay: &Replay<'_>,
    root: runtime::RunId,
    observer: &mut Observer,
    user_turns: &mut u32,
    deadline: tokio::time::Instant,
) -> (Status, String) {
    let elapsed = || {
        (
            Status::Incomplete,
            format!(
                "elapsed time budget exhausted ({} s)",
                replay.budget.max_elapsed_secs
            ),
        )
    };
    let mut handled_turns = 0;
    let mut shown = HashSet::new();
    loop {
        let (conversation, pending) = {
            let state = lock(&observer.state);
            if let Some(breach) = state.budget_breach(replay.budget) {
                return (Status::Incomplete, breach);
            }
            if state
                .root
                .as_deref()
                .is_some_and(|known| known != root.to_string())
            {
                return (
                    Status::Failed,
                    "the replay started an unexpected root run".into(),
                );
            }
            let detail = || {
                state
                    .root_reason
                    .as_deref()
                    .map_or_else(String::new, |reason| format!(": {reason}"))
            };
            match state.root_phase {
                Some(AgentRunPhase::Done) => {
                    return (
                        Status::Completed,
                        format!("the root run finished{}", detail()),
                    );
                }
                Some(AgentRunPhase::Error) => {
                    return (Status::Failed, format!("the root run failed{}", detail()));
                }
                Some(AgentRunPhase::Stopped) => {
                    return (Status::Failed, format!("the root run stopped{}", detail()));
                }
                _ => {}
            }
            let unseen = state
                .pending
                .iter()
                .any(|question| !shown.contains(&question.id));
            if state.turns_completed <= handled_turns && !unseen {
                (None, None)
            } else {
                handled_turns = state.turns_completed;
                shown.extend(state.pending.iter().map(|question| question.id.clone()));
                (
                    Some(state.conversation.clone()),
                    state.pending.first().cloned(),
                )
            }
        };
        let Some(conversation) = conversation else {
            tokio::select! {
                changed = observer.changed.changed() => if changed.is_err() {
                    return (Status::Failed, "the replay's event bus closed".into());
                },
                () = tokio::time::sleep_until(deadline) => return elapsed(),
            }
            continue;
        };
        let decision = tokio::select! {
            decision = replay.simulator.decide(&replay.thread.turns, &conversation, pending.as_ref()) => decision,
            () = tokio::time::sleep_until(deadline) => return elapsed(),
        };
        let decision = match decision {
            Ok(decision) => decision,
            Err(error) => return (Status::Failed, error.to_string()),
        };
        if decision.action == Action::Finish {
            return (
                Status::Completed,
                format!("the simulated user finished: {}", decision.reason),
            );
        }
        if *user_turns >= replay.budget.max_user_turns {
            return (
                Status::Incomplete,
                format!(
                    "user turn budget exhausted ({})",
                    replay.budget.max_user_turns
                ),
            );
        }
        let sent = match (decision.action, pending) {
            (Action::Answer, Some(question)) => replay
                .runtime
                .answer_user_question(&question.id, &decision.text)
                .map(|_| Entry::Answer {
                    question_id: question.id,
                    title: question.title,
                    text: decision.text,
                }),
            _ => replay
                .runtime
                .continue_goal(root, decision.text.clone(), replay.chat.clone())
                .map(|_| Entry::User {
                    text: decision.text,
                })
                .map_err(|error| error.to_string()),
        };
        match sent {
            Ok(entry) => {
                *user_turns += 1;
                lock(&observer.state).conversation.push(entry);
            }
            Err(error) => {
                return (
                    Status::Failed,
                    format!("the simulated user's input was rejected: {error}"),
                );
            }
        }
    }
}

async fn settle(runtime: &AgentRuntime) {
    for agent in runtime.list_agents() {
        if tokio::time::timeout(SETTLE_TIMEOUT, runtime.wait(agent.run_id))
            .await
            .is_err()
        {
            tracing::warn!(run_id = %agent.run_id, "replay run did not stop in time");
        }
    }
}

#[cfg(test)]
mod tests;
