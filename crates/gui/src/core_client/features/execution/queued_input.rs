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
        /// Where the list of the thread's receipts stands, the last way to settle a
        /// withdrawal whose command receipt the core no longer has.
        listing: Listing,
    },
    Ready(Words),
}

/// The thread's receipts read to settle a withdrawal whose outcome is unknown. They are read
/// for the thread the operation started in, whatever is selected now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Listing {
    /// The command's own receipt may still settle it (no new session yet).
    NotNeeded,
    /// A new session began and the command's receipt did not settle it: read the list.
    Wanted,
    /// The read of the list was asked for.
    Asked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Op {
    request: Request,
    /// The selection epoch the operation started in.
    epoch: u64,
    /// A frame found the composer different from the guard. It stays set when the
    /// composer is put back, so an attachment added and removed meanwhile counts too.
    touched: bool,
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
    /// The input was taken back but the composer changed meanwhile, so the composer was
    /// left alone. The words are not kept.
    TakenBackButChanged {
        request: Request,
    },
}

/// What a frame's comparison of the composer found, and what the operation wants next.
#[derive(Debug)]
pub struct Polled {
    pub poll: Poll,
    /// A request is due and only a pass of the feature sends it.
    pub wants_pass: bool,
    /// The thread whose receipts are to be read, once, to settle a lost withdrawal.
    pub receipts: Option<ThreadId>,
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
            touched: false,
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
                listing: Listing::NotNeeded,
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
                    listing: Listing::NotNeeded,
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
                unknown, listing, ..
            } => {
                // The command's receipt was asked for on reconnecting and its resolved
                // reply, if the core had one, came right after this reset. Without it
                // the list of the thread's receipts is all that is left to ask.
                if *unknown {
                    *listing = Listing::Wanted;
                }
            }
            Phase::Ready(_) => {}
        }
    }

    /// Every pass: follows the selection and, for a withdrawal whose outcome was lost and
    /// whose command receipt the core could not give, settles it from the thread's receipts
    /// once they were read. Only the receipt read after the withdrawal was sent counts: a
    /// list showing the input taken back says nothing about this command until then.
    pub fn maintain(&mut self, selected: Option<&ThreadId>, cache: &Cache) {
        if self.selected.as_ref() != selected {
            self.selected = selected.cloned();
            self.epoch += 1;
        }
        let Some(op) = self.op.as_mut() else { return };
        let Phase::Withdrawing {
            words,
            unknown: true,
            listing: Listing::Asked,
            ..
        } = &op.phase
        else {
            return;
        };
        match cache.receipt_read(op.request.thread.as_str()) {
            Some(ReceiptRead::Loaded) => {
                let known = cache.receipts.get(op.request.submission.as_str());
                if known.is_some_and(|receipt| receipt.state == InputReceiptState::Withdrawn) {
                    op.phase = Phase::Ready(words.clone());
                } else {
                    let shown = known.map_or("not listed", |receipt| state_name(receipt.state));
                    self.fail(format!(
                        "It could not be confirmed that the input was taken back: it is {shown}."
                    ));
                }
            }
            Some(ReceiptRead::Failed(reason)) => {
                let reason = reason.clone();
                self.fail(format!(
                    "It could not be confirmed that the input was taken back, and its state could not be read ({reason}). It was not sent again; look at the queue before trying again."
                ));
            }
            Some(ReceiptRead::Loading) | None => {}
        }
    }

    /// The thread whose receipts the connection is to read now, once: the operation's own,
    /// whatever is selected.
    pub fn take_receipts_wanted(&mut self) -> Option<ThreadId> {
        let op = self.op.as_mut()?;
        let Phase::Withdrawing { listing, .. } = &mut op.phase else {
            return None;
        };
        if *listing != Listing::Wanted {
            return None;
        }
        *listing = Listing::Asked;
        Some(op.request.thread.clone())
    }

    /// The frame compares the composer with the guard of the operation: `current` reads
    /// how the composer of a thread looks now. Returns what the frame is to do.
    pub fn poll(&mut self, current: impl Fn(&ThreadId) -> Guard) -> Poll {
        let Some(op) = self.op.as_mut() else {
            return Poll::Nothing;
        };
        if current(&op.request.guard.thread) != op.request.guard {
            op.touched = true;
        }
        let fit = op.epoch == self.epoch
            && self.selected.as_ref() == Some(&op.request.guard.thread)
            && !op.touched;
        match std::mem::replace(&mut op.phase, Phase::Reading { tag: None }) {
            Phase::Checking(_) | Phase::WithdrawDue(_) if !fit => {
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
                    Poll::TakenBackButChanged { request }
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
    // Then: the composer is left alone and the words are dropped with the operation
    #[test]
    fn a_composer_that_changed_is_left_alone_and_the_words_are_not_kept() {
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
                request: request(Kind::Edit)
            }
        );
        assert_eq!(queued.view(), QueueView::default());
    }

    // Given: a file added to the composer and removed again while the words are read, or
    //        while the input is taken back
    // Then: the composer counts as changed, though it looks as it did at the end
    #[test]
    fn an_attachment_added_and_removed_meanwhile_still_counts_as_a_change() {
        let with_file = |_: &ThreadId| Guard {
            files: vec!["a.png".into()],
            ..guard(3)
        };

        let mut queued = selecting();
        queued.start(request(Kind::Restore));
        queued.poll(with_file);
        read(&mut queued, InputReceiptState::Dropped);
        assert_eq!(queued.poll(unchanged), Poll::Nothing);
        assert!(queued.view().failure.is_some(), "nothing was applied");

        let mut queued = selecting();
        queued.start(request(Kind::Edit));
        read(&mut queued, InputReceiptState::Accepted);
        queued.poll(unchanged);
        queued.next_request(true);
        queued.sent(2);
        queued.poll(with_file);
        queued.withdraw_reply(2, Ok(()));
        assert!(matches!(
            queued.poll(unchanged),
            Poll::TakenBackButChanged { .. }
        ));
    }

    // Given: a composer that changes after the words were read and before the withdrawal
    //        is sent
    // Then: the withdrawal is never sent
    #[test]
    fn a_change_before_the_withdrawal_is_sent_stops_it() {
        let mut queued = selecting();
        queued.start(request(Kind::Edit));
        read(&mut queued, InputReceiptState::Accepted);
        assert_eq!(queued.poll(unchanged), Poll::Nothing);

        assert_eq!(queued.poll(typed_in), Poll::Nothing);

        assert!(queued.next_request(true).is_none());
        assert!(queued.view().failure.is_some());
    }

    // Given: a withdrawal whose reply was lost
    // Then: nothing is sent again; the core's receipt of the command settles it, and the
    //       input's own receipt is not looked at while that may still answer
    #[test]
    fn a_withdrawal_with_an_unknown_outcome_is_settled_by_the_command_receipt() {
        let mut queued = selecting();
        queued.start(request(Kind::Edit));
        read(&mut queued, InputReceiptState::Accepted);
        queued.poll(unchanged);
        queued.next_request(true);
        queued.sent(2);
        assert!(queued.withdraw_reply(2, Err(FeatureError::outcome_unknown("lost"))));
        assert!(queued.next_request(true).is_none());
        assert_eq!(queued.take_receipts_wanted(), None, "no new session yet");

        // The resolved reply for the same tag arrives right after the reset.
        queued.reset();
        assert!(!queued.withdraw_reply(2, Ok(())));
        assert!(matches!(queued.poll(unchanged), Poll::Apply { .. }));
    }

    fn receipt(state: &str) -> workbench_protocol::views::InputReceipt {
        serde_json::from_value(serde_json::json!({
            "submission_id": "s", "thread": "t", "run": "r", "state": state,
            "revision": "1", "sent_by": {"actor": "human", "surface": "desktop"},
        }))
        .unwrap()
    }

    /// An edit whose withdrawal was sent and lost, and a new session that could not settle it.
    fn lost_in_a_new_session(selected: &ThreadId) -> QueuedInputs {
        let mut queued = selecting();
        queued.start(request(Kind::Edit));
        read(&mut queued, InputReceiptState::Accepted);
        queued.poll(unchanged);
        queued.next_request(true);
        queued.sent(2);
        queued.withdraw_reply(2, Err(FeatureError::outcome_unknown("lost")));
        queued.reset();
        queued.maintain(Some(selected), &Cache::default());
        queued
    }

    // Given: another client took the input back before this client's withdrawal was refused
    // Then: the receipt list showing it taken back does not settle this client's operation,
    //       and the refusal ends it without any text reaching the composer
    #[test]
    fn a_withdrawal_by_another_client_is_not_this_clients_success() {
        let mut queued = selecting();
        queued.start(request(Kind::Edit));
        read(&mut queued, InputReceiptState::Accepted);
        queued.poll(unchanged);
        queued.next_request(true);
        queued.sent(2);
        let mut cache = Cache::default();
        cache.merge_receipt(receipt("withdrawn"));
        cache.receipts_read("t", Ok(()));
        queued.maintain(Some(&thread()), &cache);
        assert_eq!(queued.poll(unchanged), Poll::Nothing, "still waiting");
        assert!(queued.view().busy.is_some());

        let refused = FeatureError {
            code: Some(ErrorCode::Conflict),
            ..FeatureError::new("not queued")
        };
        queued.withdraw_reply(2, Err(refused));

        assert_eq!(queued.poll(unchanged), Poll::Nothing);
        assert!(queued.view().busy.is_none());
        assert!(queued.view().failure.is_some());
    }

    // Given: a lost withdrawal that the command's receipt could not settle after a new session
    // Then: the operation's own thread is read once, whatever is selected, and the list
    //       decides: taken back is done, anything else ends the operation with that state
    #[test]
    fn an_unknown_outcome_ends_from_the_origin_threads_receipts() {
        let other = ThreadId::new("other").unwrap();
        let mut cache = Cache::default();

        let mut queued = lost_in_a_new_session(&other);
        assert_eq!(queued.take_receipts_wanted(), Some(thread()));
        assert_eq!(queued.take_receipts_wanted(), None, "asked once");
        cache.receipts_loading("t");
        queued.maintain(Some(&other), &cache);
        assert!(queued.view().busy.is_some(), "waits while it is read");

        cache.merge_receipt(receipt("accepted"));
        cache.receipts_read("t", Ok(()));
        queued.maintain(Some(&other), &cache);
        assert!(queued.view().busy.is_none());
        assert!(
            queued
                .view()
                .failure
                .unwrap()
                .message
                .contains("still queued")
        );

        let mut queued = lost_in_a_new_session(&other);
        queued.take_receipts_wanted();
        cache.merge_receipt(receipt("withdrawn"));
        queued.maintain(Some(&other), &cache);
        assert!(matches!(
            queued.poll(unchanged),
            Poll::TakenBackButChanged { .. }
        ));
    }

    // Given: the same, with the receipts of the origin thread unreadable
    // Then: the unknown outcome is shown, the slot is freed and nothing is sent again
    #[test]
    fn an_unreadable_receipt_list_frees_the_operation_with_the_outcome_unknown() {
        let mut cache = Cache::default();
        let mut queued = lost_in_a_new_session(&thread());
        queued.take_receipts_wanted();
        cache.receipts_read("t", Err("offline".into()));

        queued.maintain(Some(&thread()), &cache);

        assert!(queued.view().busy.is_none());
        let message = queued.view().failure.unwrap().message;
        assert!(message.contains("could not be confirmed") && message.contains("offline"));
        assert!(queued.next_request(true).is_none());
        assert!(queued.start(request(Kind::Edit)));
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
