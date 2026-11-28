//! Edit and Restore of a queued input (ADR 0045 E7): one operation at a time that reads
//! the words of an input by id and, for Edit, takes the input back before the words reach
//! the composer.
//!
//! The operation follows the person's composer without owning it: it remembers how the
//! composer looked when the button was pressed (a [`Guard`]) and the conversation applies
//! the result only to a composer that still looks so, in the selection that asked. The
//! words are held for this one operation and dropped with it; they are never put in the
//! shared cache.
//!
//! Steps: `Reading` (the `GetInputDraft` query) → `Checking` (the frame compares the
//! composer with the guard) → for Edit `WithdrawDue` → `Withdrawing` (`WithdrawFollowUp`;
//! its outcome may be unknown, and is then settled by the core's command receipt or by the
//! input's receipt, never by sending again) → `Ready` (the frame compares the composer once
//! more). Restore skips the withdrawal: the words are only copied.
use workbench_protocol::{
    commands::WithdrawFollowUp,
    errors::ErrorCode,
    ids::{RunId, SubmissionId, ThreadId},
    namespaces::conversation::{GetInputDraft, InputDraft},
    queries::QueryReply,
    views::InputReceiptState,
};

use crate::core_client::cache::{Cache, ReceiptRead};
use crate::core_client::features::{FeatureError, FeatureReply};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An accepted input is taken back and its words put in the composer.
    Edit,
    /// The words of a dropped or unknown input are copied into the composer.
    Restore,
}

/// How the composer looked when the operation started. Any difference later means the
/// person changed something, so nothing is written over it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guard {
    pub thread: ThreadId,
    pub draft_revision: u64,
    /// The ids of the files in the composer, and how many are still being read.
    pub files: Vec<String>,
    pub loading: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Words {
    pub text: String,
    pub image_count: u32,
    pub attachment_count: u32,
}

/// What a press of Edit or Restore asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub kind: Kind,
    pub thread: ThreadId,
    pub submission: SubmissionId,
    pub run: RunId,
    pub guard: Guard,
}

/// What the queue's buttons ask of the operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    Start(Request),
    /// The failure the queue shows was read.
    Dismiss,
}

/// What the next pass sends; the caller allocates the tag and reports it with
/// [`QueuedInputs::sent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outgoing {
    Read(GetInputDraft),
    Withdraw(WithdrawFollowUp),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Reading {
        tag: Option<u64>,
    },
    Checking(Words),
    WithdrawDue(Words),
    Withdrawing {
        words: Words,
        tag: Option<u64>,
        /// The reply was lost: the core's receipt of the command settles it.
        unknown: bool,
        /// A new session started since the command was sent.
        session_changed: bool,
    },
    Ready(Words),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Op {
    request: Request,
    /// The selection epoch the operation started in.
    epoch: u64,
    phase: Phase,
}

/// Why an operation ended without changing the composer, for the row it was started on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub thread: ThreadId,
    pub submission: SubmissionId,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Reading,
    TakingBack,
}

/// What the conversation draws of the operation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QueueView {
    pub busy: Option<Busy>,
    pub failure: Option<Failure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Busy {
    pub thread: ThreadId,
    pub submission: SubmissionId,
    pub kind: Kind,
    pub stage: Stage,
}

/// What the frame found when it compared the composer with the guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll {
    Nothing,
    /// Put these words in the composer of `request.thread`.
    Apply {
        request: Request,
        words: Words,
    },
    /// The input was taken back but the composer changed meanwhile, so the words are
    /// handed over as a notice instead.
    TakenBackButChanged {
        request: Request,
        words: Words,
    },
}

#[derive(Default)]
pub struct QueuedInputs {
    op: Option<Op>,
    failure: Option<Failure>,
    /// Counts the changes of the selected thread, so a return to a thread is not the
    /// selection the operation started in.
    epoch: u64,
    selected: Option<ThreadId>,
}

impl QueuedInputs {
    pub fn view(&self) -> QueueView {
        QueueView {
            busy: self.op.as_ref().map(|op| {
                let stage = match op.phase {
                    Phase::Reading { .. } | Phase::Checking(_) => Stage::Reading,
                    Phase::WithdrawDue(_) | Phase::Withdrawing { .. } | Phase::Ready(_) => {
                        Stage::TakingBack
                    }
                };
                Busy {
                    thread: op.request.thread.clone(),
                    submission: op.request.submission.clone(),
                    kind: op.request.kind,
                    stage,
                }
            }),
            failure: self.failure.clone(),
        }
    }

    /// A request is due and only a pass of the feature sends it.
    pub fn wants_pass(&self) -> bool {
        self.op.as_ref().is_some_and(|op| {
            matches!(
                op.phase,
                Phase::Reading { tag: None } | Phase::WithdrawDue(_)
            )
        })
    }

    pub fn intent(&mut self, intent: Intent) {
        match intent {
            Intent::Start(request) => {
                self.start(request);
            }
            Intent::Dismiss => self.failure = None,
        }
    }

    /// Starts an operation unless one is in progress.
    pub fn start(&mut self, request: Request) -> bool {
        if self.op.is_some() {
            return false;
        }
        self.failure = None;
        self.op = Some(Op {
            request,
            epoch: self.epoch,
            phase: Phase::Reading { tag: None },
        });
        true
    }

    fn fail(&mut self, message: impl Into<String>) {
        if let Some(op) = self.op.take() {
            self.failure = Some(Failure {
                thread: op.request.thread,
                submission: op.request.submission,
                message: message.into(),
            });
        }
    }

    /// The request this pass sends, if one is due. Without the capability the operation
    /// ends here, saying so.
    pub fn next_request(&mut self, can_read: bool) -> Option<Outgoing> {
        let op = self.op.as_ref()?;
        let outgoing = match &op.phase {
            Phase::Reading { tag: None } => Outgoing::Read(GetInputDraft {
                thread: op.request.thread.clone(),
                submission_id: op.request.submission.clone(),
            }),
            Phase::WithdrawDue(_) => Outgoing::Withdraw(WithdrawFollowUp {
                thread: op.request.thread.clone(),
                run: op.request.run.clone(),
                submission_id: op.request.submission.clone(),
            }),
            _ => return None,
        };
        if !can_read {
            self.fail("This core cannot give back the words of a queued input.");
            return None;
        }
        Some(outgoing)
    }

    /// The request returned by [`Self::next_request`] left with `tag`.
    pub fn sent(&mut self, tag: u64) {
        let Some(op) = self.op.as_mut() else { return };
        op.phase = match std::mem::replace(&mut op.phase, Phase::Reading { tag: None }) {
            Phase::Reading { tag: None } => Phase::Reading { tag: Some(tag) },
            Phase::WithdrawDue(words) => Phase::Withdrawing {
                words,
                tag: Some(tag),
                unknown: false,
                session_changed: false,
            },
            other => other,
        };
    }

    /// The answer to the read tagged `tag`. One for another tag (an operation that ended
    /// and was replaced) is ignored.
    pub fn read_reply(&mut self, tag: u64, reply: Result<QueryReply, FeatureError>) {
        let Some(op) = self.op.as_mut() else { return };
        if op.phase != (Phase::Reading { tag: Some(tag) }) {
            return;
        }
        let request = op.request.clone();
        match reply {
            Ok(QueryReply::Conversation(
                workbench_protocol::namespaces::ConversationReply::InputDraft(draft),
            )) => match accept_draft(&request, draft) {
                Ok(words) => op.phase = Phase::Checking(words),
                Err(message) => self.fail(message),
            },
            Ok(_) => self.fail("The core answered with something else than the words."),
            Err(error) => self.fail(read_failure(&error)),
        }
    }

    /// The answer to the withdrawal tagged `tag`. Returns whether the outcome is unknown,
    /// in which case the caller keeps the tag: the resolved reply follows after the next
    /// snapshot.
    pub fn withdraw_reply(&mut self, tag: u64, reply: Result<(), FeatureError>) -> bool {
        let Some(op) = self.op.as_mut() else {
            return false;
        };
        let Phase::Withdrawing {
            words,
            tag: Some(expected),
            ..
        } = &op.phase
        else {
            return false;
        };
        if *expected != tag {
            return false;
        }
        let words = words.clone();
        match reply {
            Ok(()) => {
                op.phase = Phase::Ready(words);
                false
            }
            Err(error) if error.outcome_unknown => {
                op.phase = Phase::Withdrawing {
                    words,
                    tag: Some(tag),
                    unknown: true,
                    session_changed: false,
                };
                true
            }
            Err(error) => {
                self.fail(withdraw_failure(&error));
                false
            }
        }
    }

    /// A new snapshot replaced the projection. What was read for the old one is not
    /// trusted; a withdrawal whose outcome is unknown keeps waiting for its receipt.
    pub fn reset(&mut self) {
        let Some(op) = self.op.as_mut() else { return };
        match &mut op.phase {
            Phase::Reading { .. } | Phase::Checking(_) | Phase::WithdrawDue(_) => {
                self.fail("The connection changed before the input was read. Try again.");
            }
            Phase::Withdrawing {
                session_changed, ..
            } => *session_changed = true,
            Phase::Ready(_) => {}
        }
    }

    /// Every pass: follows the selection and settles a withdrawal whose outcome was lost
    /// from the receipts read for the new projection.
    pub fn maintain(&mut self, selected: Option<&ThreadId>, cache: &Cache) {
        if self.selected.as_ref() != selected {
            self.selected = selected.cloned();
            self.epoch += 1;
        }
        let Some(op) = self.op.as_mut() else { return };
        let Phase::Withdrawing {
            words,
            unknown,
            session_changed,
            ..
        } = &op.phase
        else {
            return;
        };
        let known = cache.receipts.get(op.request.submission.as_str());
        if known.is_some_and(|receipt| receipt.state == InputReceiptState::Withdrawn) {
            op.phase = Phase::Ready(words.clone());
            return;
        }
        if *unknown
            && *session_changed
            && cache.receipt_read(op.request.thread.as_str()) == Some(&ReceiptRead::Loaded)
        {
            let shown = known.map_or("not listed", |receipt| state_name(receipt.state));
            self.fail(format!(
                "It could not be confirmed that the input was taken back: it is {shown}."
            ));
        }
    }

    /// The frame compares the composer with the guard of the operation: `current` reads
    /// how the composer of a thread looks now. Returns what the frame is to do.
    pub fn poll(&mut self, current: impl Fn(&ThreadId) -> Guard) -> Poll {
        let Some(op) = self.op.as_mut() else {
            return Poll::Nothing;
        };
        let fit = op.epoch == self.epoch
            && self.selected.as_ref() == Some(&op.request.guard.thread)
            && current(&op.request.guard.thread) == op.request.guard;
        match std::mem::replace(&mut op.phase, Phase::Reading { tag: None }) {
            Phase::Checking(_) if !fit => {
                self.fail("The composer changed, so nothing was changed.");
                Poll::Nothing
            }
            Phase::Checking(words) => match op.request.kind {
                Kind::Edit => {
                    op.phase = Phase::WithdrawDue(words);
                    Poll::Nothing
                }
                Kind::Restore => {
                    let request = op.request.clone();
                    self.op = None;
                    Poll::Apply { request, words }
                }
            },
            Phase::Ready(words) => {
                let request = op.request.clone();
                self.op = None;
                if fit {
                    Poll::Apply { request, words }
                } else {
                    Poll::TakenBackButChanged { request, words }
                }
            }
            other => {
                op.phase = other;
                Poll::Nothing
            }
        }
    }
}

fn accept_draft(request: &Request, draft: InputDraft) -> Result<Words, String> {
    if draft.thread != request.thread
        || draft.submission_id != request.submission
        || draft.run != request.run
    {
        return Err("The core answered for another input.".into());
    }
    let fits = match request.kind {
        Kind::Edit => draft.state == InputReceiptState::Accepted,
        Kind::Restore => matches!(
            draft.state,
            InputReceiptState::Dropped | InputReceiptState::Unknown
        ),
    };
    if !fits {
        return Err(format!(
            "The input changed meanwhile: it is {}.",
            state_name(draft.state)
        ));
    }
    Ok(Words {
        text: draft.text,
        image_count: draft.image_count,
        attachment_count: draft.attachment_count,
    })
}

fn state_name(state: InputReceiptState) -> &'static str {
    match state {
        InputReceiptState::Accepted => "still queued",
        InputReceiptState::Withdrawn => "taken back",
        InputReceiptState::Delivered => "delivered",
        InputReceiptState::Dropped => "dropped",
        InputReceiptState::Unknown => "of unknown state",
    }
}

fn read_failure(error: &FeatureError) -> String {
    match error.code {
        Some(ErrorCode::NotFound) => {
            "The words of this input are not available (it was sent before they were kept, or it was deleted)."
                .into()
        }
        Some(ErrorCode::Conflict) => {
            "The input was delivered or taken back meanwhile, so it cannot be edited.".into()
        }
        Some(ErrorCode::LimitExceeded) => "The input is too large to edit.".into(),
        _ => format!("The words could not be read: {}", error.message),
    }
}

fn withdraw_failure(error: &FeatureError) -> String {
    match error.code {
        Some(ErrorCode::Conflict | ErrorCode::NotFound) => {
            "The agent already took this input, so it cannot be taken back.".into()
        }
        _ => format!("The input could not be taken back: {}", error.message),
    }
}

/// How a core's reply to the withdrawal command reads to the operation.
pub fn withdraw_result(reply: FeatureReply) -> Result<(), FeatureError> {
    match reply {
        FeatureReply::Command(Ok(_)) => Ok(()),
        FeatureReply::Command(Err(error)) | FeatureReply::Query(Err(error)) => Err(error),
        FeatureReply::Query(Ok(_)) => Err(FeatureError::new("unexpected reply")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workbench_protocol::namespaces::ConversationReply;

    fn thread() -> ThreadId {
        ThreadId::new("t").unwrap()
    }

    fn request(kind: Kind) -> Request {
        Request {
            kind,
            thread: thread(),
            submission: SubmissionId::new("s").unwrap(),
            run: RunId::new("r").unwrap(),
            guard: guard(3),
        }
    }

    fn guard(draft_revision: u64) -> Guard {
        Guard {
            thread: thread(),
            draft_revision,
            files: Vec::new(),
            loading: 0,
        }
    }

    fn draft(state: InputReceiptState) -> Result<QueryReply, FeatureError> {
        Ok(QueryReply::Conversation(ConversationReply::InputDraft(
            InputDraft {
                thread: thread(),
                submission_id: SubmissionId::new("s").unwrap(),
                run: RunId::new("r").unwrap(),
                state,
                text: "words".into(),
                image_count: 1,
                attachment_count: 2,
            },
        )))
    }

    fn words() -> Words {
        Words {
            text: "words".into(),
            image_count: 1,
            attachment_count: 2,
        }
    }

    /// A selected thread, the way `maintain` records it.
    fn selecting() -> QueuedInputs {
        let mut queued = QueuedInputs::default();
        queued.maintain(Some(&thread()), &Cache::default());
        queued
    }

    fn read(queued: &mut QueuedInputs, state: InputReceiptState) {
        assert!(matches!(queued.next_request(true), Some(Outgoing::Read(_))));
        queued.sent(1);
        queued.read_reply(1, draft(state));
    }

    fn unchanged(_: &ThreadId) -> Guard {
        guard(3)
    }

    fn typed_in(_: &ThreadId) -> Guard {
        guard(4)
    }

    // Given: a restore of a dropped input
    // Then: the words are read once, compared with the composer, and copied without any
    //       withdrawal
    #[test]
    fn a_restore_copies_the_words_after_one_read_and_one_check() {
        let mut queued = selecting();
        assert!(queued.start(request(Kind::Restore)));
        read(&mut queued, InputReceiptState::Dropped);
        assert!(queued.next_request(true).is_none());

        let polled = queued.poll(unchanged);

        assert_eq!(
            polled,
            Poll::Apply {
                request: request(Kind::Restore),
                words: words()
            }
        );
        assert_eq!(queued.view(), QueueView::default());
    }

    // Given: an edit
    // Then: the composer is changed only after the withdrawal was confirmed
    #[test]
    fn an_edit_changes_the_composer_only_after_the_withdrawal_succeeded() {
        let mut queued = selecting();
        queued.start(request(Kind::Edit));
        read(&mut queued, InputReceiptState::Accepted);
        assert_eq!(queued.poll(unchanged), Poll::Nothing);
        let Some(Outgoing::Withdraw(withdraw)) = queued.next_request(true) else {
            panic!("the withdrawal is due once the composer was checked");
        };
        assert_eq!(withdraw.submission_id.as_str(), "s");
        queued.sent(2);
        assert_eq!(queued.poll(unchanged), Poll::Nothing);

        assert!(!queued.withdraw_reply(2, Ok(())));

        assert_eq!(
            queued.poll(unchanged),
            Poll::Apply {
                request: request(Kind::Edit),
                words: words()
            }
        );
    }

    // Given: an edit whose withdrawal the core refuses
    // Then: nothing is applied and the failure names why
    #[test]
    fn a_refused_withdrawal_applies_nothing() {
        let mut queued = selecting();
        queued.start(request(Kind::Edit));
        read(&mut queued, InputReceiptState::Accepted);
        queued.poll(unchanged);
        queued.next_request(true);
        queued.sent(2);
        let refused = FeatureError {
            code: Some(ErrorCode::Conflict),
            ..FeatureError::new("adopted")
        };

        queued.withdraw_reply(2, Err(refused));

        assert_eq!(queued.poll(unchanged), Poll::Nothing);
        let view = queued.view();
        assert!(view.busy.is_none());
        assert!(view.failure.unwrap().message.contains("already took"));
    }

    // Given: the composer changed, the selection moved away and came back, or the
    //        projection was replaced while the words were being read
    // Then: the answer is not applied
    #[test]
    fn a_late_answer_is_not_applied_to_a_composer_or_selection_that_changed() {
        let mut queued = selecting();
        queued.start(request(Kind::Restore));
        read(&mut queued, InputReceiptState::Dropped);
        assert_eq!(queued.poll(typed_in), Poll::Nothing);
        assert!(queued.view().failure.is_some());

        let mut queued = selecting();
        queued.start(request(Kind::Restore));
        read(&mut queued, InputReceiptState::Dropped);
        queued.maintain(None, &Cache::default());
        queued.maintain(Some(&thread()), &Cache::default());
        assert_eq!(
            queued.poll(unchanged),
            Poll::Nothing,
            "returned to the thread"
        );
        assert!(queued.view().failure.is_some());

        let mut queued = selecting();
        queued.start(request(Kind::Restore));
        queued.next_request(true);
        queued.sent(1);
        queued.reset();
        queued.read_reply(1, draft(InputReceiptState::Dropped));
        assert_eq!(queued.poll(unchanged), Poll::Nothing);
        assert!(queued.view().failure.is_some());
    }

    // Given: a withdrawal that succeeded after the person typed in the composer
    // Then: the composer is left alone and the words are handed over for a notice
    #[test]
    fn words_taken_back_for_a_composer_that_changed_are_handed_over() {
        let mut queued = selecting();
        queued.start(request(Kind::Edit));
        read(&mut queued, InputReceiptState::Accepted);
        queued.poll(unchanged);
        queued.next_request(true);
        queued.sent(2);
        queued.withdraw_reply(2, Ok(()));

        assert_eq!(
            queued.poll(typed_in),
            Poll::TakenBackButChanged {
                request: request(Kind::Edit),
                words: words()
            }
        );
    }

    // Given: a withdrawal whose reply was lost
    // Then: nothing is sent again; the operation waits for the core's receipt of the
    //       command or, after a new session, for the input's own receipt
    #[test]
    fn a_withdrawal_with_an_unknown_outcome_is_settled_by_receipts_never_sent_again() {
        let mut queued = selecting();
        queued.start(request(Kind::Edit));
        read(&mut queued, InputReceiptState::Accepted);
        queued.poll(unchanged);
        queued.next_request(true);
        queued.sent(2);
        assert!(queued.withdraw_reply(2, Err(FeatureError::outcome_unknown("lost"))));
        assert!(queued.next_request(true).is_none());

        // The resolved reply for the same tag arrives after the reset.
        queued.reset();
        assert!(!queued.withdraw_reply(2, Ok(())));
        assert!(matches!(queued.poll(unchanged), Poll::Apply { .. }));
    }

    #[test]
    fn an_unknown_outcome_ends_from_the_receipt_the_new_session_read() {
        let receipt = |state: &str| -> workbench_protocol::views::InputReceipt {
            serde_json::from_value(serde_json::json!({
                "submission_id": "s", "thread": "t", "run": "r", "state": state,
                "revision": "1", "sent_by": {"actor": "human", "surface": "desktop"},
            }))
            .unwrap()
        };
        let lost = |queued: &mut QueuedInputs| {
            queued.start(request(Kind::Edit));
            read(queued, InputReceiptState::Accepted);
            queued.poll(unchanged);
            queued.next_request(true);
            queued.sent(2);
            queued.withdraw_reply(2, Err(FeatureError::outcome_unknown("lost")));
            queued.reset();
        };
        let mut cache = Cache::default();

        // Still waiting while the new session's receipts are not read.
        let mut queued = selecting();
        lost(&mut queued);
        queued.maintain(Some(&thread()), &cache);
        assert!(queued.view().busy.is_some());

        // Read and still queued: the withdrawal did not happen, so the operation ends.
        cache.merge_receipt(receipt("accepted"));
        cache.receipts_read("t", Ok(()));
        queued.maintain(Some(&thread()), &cache);
        assert!(queued.view().busy.is_none());
        assert!(
            queued
                .view()
                .failure
                .unwrap()
                .message
                .contains("still queued")
        );

        // Taken back: it counts as done.
        let mut queued = selecting();
        lost(&mut queued);
        cache.merge_receipt(receipt("withdrawn"));
        queued.maintain(Some(&thread()), &cache);
        assert!(matches!(queued.poll(unchanged), Poll::Apply { .. }));
    }

    // Given: a core that refuses or does not know the input
    // Then: the operation ends with the reason and a second one may start
    #[test]
    fn a_failed_read_ends_the_operation_and_frees_the_next() {
        for (code, expected) in [
            (Some(ErrorCode::NotFound), "not available"),
            (Some(ErrorCode::Conflict), "delivered or taken back"),
            (Some(ErrorCode::LimitExceeded), "too large"),
            (None, "could not be read"),
        ] {
            let mut queued = selecting();
            queued.start(request(Kind::Edit));
            queued.next_request(true);
            queued.sent(1);
            let error = FeatureError {
                code,
                ..FeatureError::new("offline")
            };
            queued.read_reply(1, Err(error));
            assert!(queued.view().failure.unwrap().message.contains(expected));
            assert!(queued.start(request(Kind::Edit)));
        }
        let mut queued = selecting();
        assert!(queued.start(request(Kind::Edit)));
        assert!(!queued.start(request(Kind::Restore)), "one at a time");
    }

    // Given: an answer whose state does not fit the button pressed
    // Then: the operation ends without touching anything
    #[test]
    fn an_answer_in_another_state_than_the_button_expects_is_refused() {
        let mut queued = selecting();
        queued.start(request(Kind::Edit));
        read(&mut queued, InputReceiptState::Dropped);
        assert!(queued.view().failure.is_some());
        assert_eq!(queued.poll(unchanged), Poll::Nothing);
    }

    #[test]
    fn a_core_without_the_capability_ends_the_operation_at_once() {
        let mut queued = selecting();
        queued.start(request(Kind::Edit));
        assert!(queued.next_request(false).is_none());
        assert!(queued.view().failure.is_some());
    }
}
