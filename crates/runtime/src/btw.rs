//! `/btw`: one tool-less side question answered from a run's frozen context.
//!
//! The model boundary publishes the request it is about to send for a run, and the
//! loop adds the final assistant message once a turn settles. A side question copies
//! that immutable snapshot, appends one user message and sends it once, on its own
//! task. It never touches the run's context, route pin, replay state, turn index or
//! persistence, and the answer goes only to the caller.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use providers::{
    ChatRequest, ContentBlock, FinishReason, Message, ObservationContext, ProviderAuth,
    ProviderClient, RequestTurnContext, Role, ToolChoice,
};

use crate::RunId;

const FRAMING: &str = "This is a one-off side question, not part of the conversation above. \
Answer it briefly from what is already in the conversation. You have no tool access here, so do \
not call tools. Neither this question nor your answer will be kept in the conversation.";

/// Once the registry holds more runs than this, the oldest slot that no live run
/// holds is released. A running run's slot is never evicted, so the bound is soft.
const MAX_SLOTS: usize = 64;

/// Why a side question could not be answered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BtwError {
    #[error("this thread has no conversation context in this process")]
    NoLocalContext,
    #[error("another window or process now holds this thread")]
    OwnedElsewhere,
    #[error("the conversation has not sent its first request yet")]
    ContextNotReady,
    #[error("the conversation's provider cannot answer without tools")]
    UnsupportedProvider,
    #[error("the conversation is too large to ask a side question")]
    ContextTooLarge,
    #[error("the provider request failed: {0}")]
    ProviderFailed(String),
    #[error("the model tried to call a tool; side questions run without tools")]
    UnexpectedToolCall,
    #[error("the model returned no answer")]
    EmptyAnswer,
}

/// A side question's answer and which context it was drawn from.
#[derive(Clone, PartialEq, Eq)]
pub struct BtwAnswer {
    pub text: String,
    pub source: BtwSource,
}

impl std::fmt::Debug for BtwAnswer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BtwAnswer")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

/// The context a side question was answered from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtwSource {
    pub run_id: String,
    /// Whether the snapshot already holds the run's last final answer. When it does
    /// not, the newest tool round or reply is missing and the latest request that
    /// was sent to the model is used instead.
    pub settled: bool,
}

/// An immutable copy of what the model last saw for one run.
pub struct BtwSnapshot {
    source_run_id: String,
    generation: u64,
    request: ChatRequest,
    client: Arc<dyn ProviderClient>,
    auth: ProviderAuth,
    context_window: Option<u64>,
    final_assistant: Option<Message>,
}

impl std::fmt::Debug for BtwSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BtwSnapshot")
            .field("source_run_id", &self.source_run_id)
            .field("generation", &self.generation)
            .field("messages", &self.request.messages.len())
            .field("settled", &self.final_assistant.is_some())
            .finish_non_exhaustive()
    }
}

impl BtwSnapshot {
    pub fn source(&self) -> BtwSource {
        BtwSource {
            run_id: self.source_run_id.clone(),
            settled: self.final_assistant.is_some(),
        }
    }

    /// The one request a side question sends. Everything before the new user
    /// message is the snapshot unchanged.
    pub(crate) fn question_request(&self, question: &str) -> ChatRequest {
        let mut request = self.request.clone();
        request.messages.extend(self.final_assistant.clone());
        request.messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: format!("{FRAMING}\n\n{question}"),
            }],
        });
        request.tool_choice = Some(ToolChoice::None);
        request.output_schema = None;
        let is_subagent = self
            .request
            .observation
            .as_ref()
            .and_then(|observation| observation.turn.as_ref())
            .is_some_and(|turn| turn.is_subagent);
        request.observation = Some(ObservationContext {
            attempts: None,
            turn: Some(RequestTurnContext::unattributed(is_subagent)),
            run_id: self.source_run_id.clone(),
            purpose: event_bus::RequestPurpose::Btw,
        });
        request
    }

    /// Sends the question once. Dropping the future cancels the request.
    ///
    /// # Errors
    /// See [`BtwError`]; a tool call in the reply is refused, never run.
    pub async fn ask(&self, question: &str) -> Result<BtwAnswer, BtwError> {
        if !self.client.supports_tool_choice_none(&self.request).await {
            return Err(BtwError::UnsupportedProvider);
        }
        let request = self.question_request(question);
        if let Some(window) = self.context_window
            && crate::compaction::estimator::estimate_tokens(&request.messages) >= window
        {
            return Err(BtwError::ContextTooLarge);
        }
        let response = self
            .client
            .send(&self.auth, &request)
            .await
            .map_err(|error| BtwError::ProviderFailed(self.scrub(&error.to_string())))?;
        if response.finish_reason == FinishReason::ToolUse
            || response
                .message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
        {
            return Err(BtwError::UnexpectedToolCall);
        }
        let text: String = response
            .message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        if text.trim().is_empty() {
            return Err(BtwError::EmptyAnswer);
        }
        Ok(BtwAnswer {
            text,
            source: self.source(),
        })
    }

    fn scrub(&self, text: &str) -> String {
        let text = if self.auth.api_key.is_empty() {
            text.to_owned()
        } else {
            text.replace(&self.auth.api_key, "***")
        };
        text.chars().take(500).collect()
    }
}

#[derive(Default)]
pub(crate) struct BtwSlot {
    state: Mutex<SlotState>,
}

#[derive(Default)]
struct SlotState {
    generation: u64,
    current: Option<Arc<BtwSnapshot>>,
}

impl BtwSlot {
    fn lock(&self) -> std::sync::MutexGuard<'_, SlotState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn current(&self) -> Option<Arc<BtwSnapshot>> {
        self.lock().current.clone()
    }
}

/// The handle a model call receives to publish the request it sends. Publishing
/// replaces one `Arc`; no request is ever shared mutably.
#[derive(Clone)]
pub struct BtwPublisher {
    run_id: String,
    slot: Arc<BtwSlot>,
}

impl std::fmt::Debug for BtwPublisher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BtwPublisher")
            .field("run_id", &self.run_id)
            .finish()
    }
}

/// A publisher is a side channel: it never makes two invocation contexts differ.
impl PartialEq for BtwPublisher {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for BtwPublisher {}

impl BtwPublisher {
    pub(crate) fn new(run_id: String, slot: Arc<BtwSlot>) -> Self {
        Self { run_id, slot }
    }

    /// Records the request about to be sent, with the client that sends it.
    pub fn publish(
        &self,
        request: &ChatRequest,
        client: Arc<dyn ProviderClient>,
        auth: ProviderAuth,
        context_window: Option<u64>,
    ) {
        let mut request = request.clone();
        request.output_schema = None;
        request.tool_choice = None;
        let mut state = self.slot.lock();
        state.generation += 1;
        let generation = state.generation;
        state.current = Some(Arc::new(BtwSnapshot {
            source_run_id: self.run_id.clone(),
            generation,
            request,
            client,
            auth,
            context_window,
            final_assistant: None,
        }));
    }

    /// Adds the run's final answer to the newest snapshot, so a question asked
    /// after the turn sees it. Tool-call turns never reach this.
    pub fn settle(&self, message: Message) {
        let mut state = self.slot.lock();
        let Some(current) = state.current.take() else {
            return;
        };
        state.current = Some(Arc::new(BtwSnapshot {
            source_run_id: current.source_run_id.clone(),
            generation: current.generation,
            request: current.request.clone(),
            client: Arc::clone(&current.client),
            auth: current.auth.clone(),
            context_window: current.context_window,
            final_assistant: Some(message),
        }));
    }
}

/// Per-run slots, kept after a run ends so a finished or stopped conversation can
/// still be asked about, and released with the runtime or by [`BtwRegistry::forget`].
#[derive(Default)]
pub(crate) struct BtwRegistry {
    slots: Mutex<Slots>,
}

#[derive(Default)]
struct Slots {
    by_run: HashMap<RunId, Arc<BtwSlot>>,
    oldest_first: Vec<RunId>,
}

impl BtwRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, Slots> {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn publisher(&self, run: RunId) -> BtwPublisher {
        let mut guard = self.lock();
        let Slots {
            by_run,
            oldest_first,
        } = &mut *guard;
        let slot = if let Some(slot) = by_run.get(&run) {
            Arc::clone(slot)
        } else {
            let slot = Arc::new(BtwSlot::default());
            by_run.insert(run, Arc::clone(&slot));
            oldest_first.push(run);
            if oldest_first.len() > MAX_SLOTS
                && let Some(position) = oldest_first.iter().position(|candidate| {
                    by_run
                        .get(candidate)
                        .is_some_and(|s| Arc::strong_count(s) == 1)
                })
            {
                let evicted = oldest_first.remove(position);
                by_run.remove(&evicted);
            }
            slot
        };
        BtwPublisher::new(run.to_string(), slot)
    }

    pub(crate) fn snapshot(&self, run: RunId) -> Result<Arc<BtwSnapshot>, BtwError> {
        let slot = self
            .lock()
            .by_run
            .get(&run)
            .cloned()
            .ok_or(BtwError::NoLocalContext)?;
        slot.current().ok_or(BtwError::ContextNotReady)
    }

    pub(crate) fn forget(&self, run: RunId) {
        let mut guard = self.lock();
        guard.by_run.remove(&run);
        guard.oldest_first.retain(|candidate| *candidate != run);
    }
}

impl crate::AgentRuntime {
    /// The frozen context of `run` for a side question.
    ///
    /// # Errors
    /// [`BtwError::NoLocalContext`] for a run this process holds no context for,
    /// [`BtwError::ContextNotReady`] before its first request.
    pub fn btw_snapshot(&self, run: RunId) -> Result<Arc<BtwSnapshot>, BtwError> {
        if self.deny_deleted_run(&run.to_string()).is_err() {
            self.shared.btw.forget(run);
            return Err(BtwError::NoLocalContext);
        }
        self.shared.btw.snapshot(run)
    }

    /// Releases the context kept for `run`.
    pub fn forget_btw(&self, run: RunId) {
        self.shared.btw.forget(run);
    }

    pub(crate) fn btw_publisher(&self, run: RunId) -> BtwPublisher {
        self.shared.btw.publisher(run)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots(registry: &BtwRegistry) -> usize {
        registry.lock().by_run.len()
    }

    #[test]
    fn idle_slots_are_released_oldest_first_beyond_the_cap() {
        let registry = BtwRegistry::default();
        for index in 0..(MAX_SLOTS as u64 + 10) {
            drop(registry.publisher(RunId::new(index)));
        }
        assert_eq!(slots(&registry), MAX_SLOTS);
        assert_eq!(
            registry.snapshot(RunId::new(0)).unwrap_err(),
            BtwError::NoLocalContext
        );
        let newest = RunId::new(MAX_SLOTS as u64 + 9);
        assert_eq!(
            registry.snapshot(newest).unwrap_err(),
            BtwError::ContextNotReady
        );
    }

    #[test]
    fn a_slot_a_live_run_holds_is_never_evicted() {
        let registry = BtwRegistry::default();
        let live: Vec<_> = (0..(MAX_SLOTS as u64 + 10))
            .map(|index| registry.publisher(RunId::new(index)))
            .collect();
        assert_eq!(
            slots(&registry),
            live.len(),
            "the cap is soft for live runs"
        );
        assert_eq!(
            registry.snapshot(RunId::new(0)).unwrap_err(),
            BtwError::ContextNotReady
        );
    }

    #[test]
    fn forgetting_a_run_drops_only_that_slot() {
        let registry = BtwRegistry::default();
        drop(registry.publisher(RunId::new(1)));
        drop(registry.publisher(RunId::new(2)));
        registry.forget(RunId::new(1));
        assert_eq!(
            registry.snapshot(RunId::new(1)).unwrap_err(),
            BtwError::NoLocalContext
        );
        assert_eq!(
            registry.snapshot(RunId::new(2)).unwrap_err(),
            BtwError::ContextNotReady
        );
    }
}
