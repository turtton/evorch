//! The side question (`/btw`) each thread may show above its composer.
//!
//! The composer sends `AskBtw` as an ordinary command (its draft goes when the core
//! accepts) and registers the operation here. What the core holds is read with
//! `GetBtw`: once after the registration (a refused question has nothing to read,
//! which the card says), when `BtwChanged` names the operation, and after a new
//! snapshot replaced the projection. The state lives in this window's memory only.
use std::collections::BTreeMap;

use workbench_protocol::{
    errors::ErrorCode,
    ids::{OperationId, ThreadId},
    namespaces::{
        ExecutionEvent, ExecutionReply,
        execution::{BtwSource, BtwView},
    },
    operation::{CancelReason, OperationState},
    queries::QueryReply,
};

use crate::core_client::features::FeatureError;

const REFUSED_OR_LOST: &str = "The core has no record of this question: it was refused (the status line says why) or the core restarted. Ask again.";

#[derive(Debug, Clone, PartialEq)]
pub enum CardState {
    Asking,
    Answered {
        text: String,
        truncated: bool,
        source: Option<BtwSource>,
    },
    Failed(String),
}

struct Card {
    operation: OperationId,
    question: String,
    state: CardState,
    read_due: bool,
}

/// What the conversation draws for the selected thread.
#[derive(Debug, Clone, Copy)]
pub struct BtwCardView<'a> {
    pub operation: &'a OperationId,
    pub question: &'a str,
    pub state: &'a CardState,
}

impl BtwCardView<'_> {
    pub fn asking(&self) -> bool {
        matches!(self.state, CardState::Asking)
    }
}

/// A read or a cancel the feature's next pass sends.
#[derive(Default)]
pub struct Outgoing {
    pub reads: Vec<(ThreadId, OperationId)>,
    pub cancels: Vec<OperationId>,
}

#[derive(Default)]
pub struct SideQuestions {
    cards: BTreeMap<ThreadId, Card>,
    cancels: Vec<OperationId>,
}

impl SideQuestions {
    /// The composer sent `question` as `operation`; it replaces what the thread showed.
    pub fn ask(&mut self, thread: ThreadId, operation: OperationId, question: String) {
        self.cards.insert(
            thread,
            Card {
                operation,
                question,
                state: CardState::Asking,
                read_due: true,
            },
        );
    }

    /// The card is dropped; a question still being answered is cancelled in the core.
    pub fn close(&mut self, thread: &ThreadId) {
        if let Some(card) = self.cards.remove(thread)
            && card.state == CardState::Asking
        {
            self.cancels.push(card.operation);
        }
    }

    pub fn card(&self, thread: &ThreadId) -> Option<BtwCardView<'_>> {
        self.cards.get(thread).map(|card| BtwCardView {
            operation: &card.operation,
            question: &card.question,
            state: &card.state,
        })
    }

    pub fn event(&mut self, event: &ExecutionEvent) {
        let ExecutionEvent::BtwChanged { thread, operation } = event else {
            return;
        };
        if let Some(card) = self.cards.get_mut(thread)
            && card.operation == operation.operation_id
            && card.state == CardState::Asking
        {
            card.read_due = true;
        }
    }

    /// A new snapshot replaced the projection: a question still asked may have settled
    /// while events were missed.
    pub fn reset(&mut self) {
        for card in self.cards.values_mut() {
            card.read_due = card.state == CardState::Asking;
        }
    }

    pub fn take_outgoing(&mut self) -> Outgoing {
        let reads = self
            .cards
            .iter_mut()
            .filter_map(|(thread, card)| {
                std::mem::take(&mut card.read_due).then(|| (thread.clone(), card.operation.clone()))
            })
            .collect();
        Outgoing {
            reads,
            cancels: std::mem::take(&mut self.cancels),
        }
    }

    /// The answer to a read. A reply for a card that was closed or replaced is ignored.
    pub fn read(
        &mut self,
        thread: &ThreadId,
        operation: &OperationId,
        reply: Result<QueryReply, FeatureError>,
    ) {
        let Some(card) = self
            .cards
            .get_mut(thread)
            .filter(|card| &card.operation == operation)
        else {
            return;
        };
        match reply {
            Ok(QueryReply::Execution(ExecutionReply::Btw(view)))
                if &view.operation.operation_id == operation =>
            {
                card.state = state_of(*view);
            }
            Ok(_) => {}
            Err(error) => match error.code {
                Some(ErrorCode::NotFound) => card.state = CardState::Failed(REFUSED_OR_LOST.into()),
                Some(_) => card.state = CardState::Failed(error.message),
                // The question was not asked of the core at all: the next snapshot reads it.
                None => {}
            },
        }
    }
}

fn state_of(view: BtwView) -> CardState {
    match view.operation.state {
        OperationState::Pending => CardState::Asking,
        OperationState::Ready => CardState::Answered {
            text: view.answer.unwrap_or_default(),
            truncated: view.truncated,
            source: view.source,
        },
        OperationState::Failed { .. } => CardState::Failed(
            view.failure
                .unwrap_or_else(|| "The side question failed.".into()),
        ),
        OperationState::Cancelled { reason } => CardState::Failed(
            match reason {
                CancelReason::Requested => "The side question was cancelled.",
                CancelReason::Restart => "The core restarted before it was answered.",
                CancelReason::Shutdown => "The core stopped before it was answered.",
            }
            .into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workbench_protocol::{operation::OperationView, wire::Revision};

    fn thread() -> ThreadId {
        ThreadId::new("t").unwrap()
    }

    fn operation(name: &str) -> OperationId {
        OperationId::new(name).unwrap()
    }

    fn view(name: &str, state: OperationState) -> BtwView {
        BtwView {
            operation: OperationView {
                operation_id: operation(name),
                state,
                revision: Revision(1),
                result: None,
            },
            thread: thread(),
            source: None,
            answer: None,
            truncated: false,
            failure: None,
        }
    }

    fn reply(view: BtwView) -> Result<QueryReply, FeatureError> {
        Ok(QueryReply::Execution(ExecutionReply::Btw(Box::new(view))))
    }

    fn changed(name: &str) -> ExecutionEvent {
        ExecutionEvent::BtwChanged {
            thread: thread(),
            operation: view(name, OperationState::Ready).operation,
        }
    }

    fn asked(name: &str) -> SideQuestions {
        let mut side = SideQuestions::default();
        side.ask(thread(), operation(name), "why?".into());
        side
    }

    // Given: a question just registered
    // Then: it is read once, then only when its operation changes or a new snapshot comes
    #[test]
    fn a_question_is_read_when_registered_changed_or_after_a_new_snapshot() {
        let mut side = asked("a");
        assert_eq!(side.take_outgoing().reads.len(), 1);
        assert!(side.take_outgoing().reads.is_empty());

        side.event(&changed("other"));
        assert!(side.take_outgoing().reads.is_empty());
        side.event(&changed("a"));
        assert_eq!(side.take_outgoing().reads.len(), 1);

        side.reset();
        assert_eq!(side.take_outgoing().reads.len(), 1);
        side.read(
            &thread(),
            &operation("a"),
            reply(view("a", OperationState::Ready)),
        );
        side.reset();
        assert!(
            side.take_outgoing().reads.is_empty(),
            "a settled card is not read again"
        );
    }

    // Given: a registered question
    // When: the core answers, fails it or does not know it
    // Then: the card shows the answer, the failure text or the lost-question text
    #[test]
    fn a_read_settles_the_card() {
        let mut side = asked("a");
        let mut ready = view("a", OperationState::Ready);
        ready.answer = Some("**done**".into());
        ready.truncated = true;
        side.read(&thread(), &operation("a"), reply(ready));
        assert_eq!(
            side.card(&thread()).unwrap().state,
            &CardState::Answered {
                text: "**done**".into(),
                truncated: true,
                source: None
            }
        );

        let mut side = asked("a");
        let mut failed = view(
            "a",
            OperationState::Failed {
                code: ErrorCode::Internal,
            },
        );
        failed.failure = Some("the provider request failed".into());
        side.read(&thread(), &operation("a"), reply(failed));
        assert_eq!(
            side.card(&thread()).unwrap().state,
            &CardState::Failed("the provider request failed".into())
        );

        let mut side = asked("a");
        let missing = FeatureError {
            code: Some(ErrorCode::NotFound),
            ..FeatureError::new("unknown side question")
        };
        side.read(&thread(), &operation("a"), Err(missing));
        assert_eq!(
            side.card(&thread()).unwrap().state,
            &CardState::Failed(REFUSED_OR_LOST.into())
        );

        let mut side = asked("a");
        side.read(
            &thread(),
            &operation("a"),
            Err(FeatureError::new("offline")),
        );
        assert!(side.card(&thread()).unwrap().asking());
    }

    // Given: a question that was closed and another asked in its place
    // Then: a late reply or event of the closed one does not bring it back, and closing a
    //       question still being answered cancels it in the core
    #[test]
    fn a_closed_question_stays_closed_and_is_cancelled() {
        let mut side = asked("a");
        side.close(&thread());
        assert!(side.card(&thread()).is_none());
        side.read(
            &thread(),
            &operation("a"),
            reply(view("a", OperationState::Ready)),
        );
        side.event(&changed("a"));
        assert!(side.card(&thread()).is_none());
        let outgoing = side.take_outgoing();
        assert!(outgoing.reads.is_empty());
        assert_eq!(outgoing.cancels, [operation("a")]);

        let mut side = asked("a");
        side.ask(thread(), operation("b"), "again?".into());
        side.read(
            &thread(),
            &operation("a"),
            reply(view("a", OperationState::Ready)),
        );
        assert_eq!(side.card(&thread()).unwrap().operation, &operation("b"));
        assert!(side.card(&thread()).unwrap().asking());

        let mut side = asked("a");
        side.read(
            &thread(),
            &operation("a"),
            reply(view("a", OperationState::Ready)),
        );
        side.close(&thread());
        assert!(side.take_outgoing().cancels.is_empty());
    }
}
