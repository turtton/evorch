//! The client side of a read operation (ADR 0045), shared by the four read
//! features of PR 13 (`diagnostics`, `memory`, `improvement`, `evaluation`).
//!
//! The core answers a read ask at once with the operation's view; the data comes
//! with the view once the operation is `Ready`. The client names an operation
//! with an id of its own and, when the area's `…_operation_changed` event says the
//! operation settled, asks the *same* request again under the same id: the core
//! answers it from the operation it kept. A reply or an event can arrive in
//! either order, so a `Pending` reply never undoes a settled one.
//!
//! A [`Read`] is the state of one such read. Its generation is bumped whenever the
//! read is begun again or forgotten, so an answer to an earlier ask (another
//! project, another group, a connection that was since replaced) is dropped
//! instead of overwriting what the screen shows now.
use workbench_protocol::{
    errors::ErrorCode,
    ids::OperationId,
    operation::{CancelReason, OperationState, OperationView},
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum Phase {
    /// Nothing was asked.
    #[default]
    Idle,
    /// Asked; the core has not said the data is ready.
    Waiting,
    Ready,
    /// The read failed or was cancelled; the text is shown as it is.
    Failed(String),
}

#[derive(Debug, Default)]
pub(crate) struct Read {
    nonce: String,
    counter: u64,
    generation: u64,
    id: Option<OperationId>,
    ask_again: bool,
    phase: Phase,
}

impl Read {
    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Start a new read of `area`: a fresh operation id, and the generation the
    /// answers to this ask must carry.
    pub fn begin(&mut self, area: &str) -> OperationId {
        if self.nonce.is_empty() {
            self.nonce = uuid::Uuid::new_v4().simple().to_string();
        }
        self.counter += 1;
        self.generation += 1;
        let id = OperationId::new(format!("{area}-{}-{}", self.nonce, self.counter))
            .expect("an operation id of the client's own making is valid");
        self.id = Some(id.clone());
        self.ask_again = false;
        self.phase = Phase::Waiting;
        id
    }

    /// Drop the read: what the core answers to it later is not wanted.
    pub fn forget(&mut self) {
        self.generation += 1;
        self.id = None;
        self.ask_again = false;
        self.phase = Phase::Idle;
    }

    /// The area's event for an operation. A settled operation of this read is
    /// to be asked for again.
    pub fn event(&mut self, view: &OperationView) {
        if self.phase == Phase::Waiting
            && self.id.as_ref() == Some(&view.operation_id)
            && view.state.is_final()
        {
            self.ask_again = true;
        }
    }

    /// Whether the same request is to be asked again now (once per settling).
    pub fn take_ask_again(&mut self) -> bool {
        std::mem::take(&mut self.ask_again)
    }

    /// An answer of the ask made at `generation`. Returns the data when this
    /// answer carries it for the current read; sets the phase otherwise.
    pub fn accept<T>(
        &mut self,
        generation: u64,
        view: &OperationView,
        data: Option<T>,
    ) -> Option<T> {
        if generation != self.generation || self.id.as_ref() != Some(&view.operation_id) {
            return None;
        }
        match view.state {
            OperationState::Pending => {
                // A settled answer may have come first; keep it.
                None
            }
            OperationState::Ready => match data {
                Some(data) => {
                    self.phase = Phase::Ready;
                    Some(data)
                }
                None => {
                    self.ask_again = true;
                    None
                }
            },
            OperationState::Failed { code } => {
                self.phase = Phase::Failed(failure_text(code));
                None
            }
            OperationState::Cancelled { reason } => {
                self.phase = Phase::Failed(cancel_text(reason));
                None
            }
        }
    }

    /// The ask itself was refused or lost.
    pub fn fail(&mut self, generation: u64, message: String) {
        if generation == self.generation && self.phase != Phase::Idle {
            self.phase = Phase::Failed(message);
        }
    }
}

fn failure_text(code: ErrorCode) -> String {
    match code {
        ErrorCode::NotFound => "Not found.".into(),
        ErrorCode::StorageUnavailable => "The storage could not be read.".into(),
        other => format!("The read failed ({other:?})."),
    }
}

fn cancel_text(reason: CancelReason) -> String {
    match reason {
        CancelReason::Requested => "The read was cancelled.".into(),
        CancelReason::Restart => "The read was interrupted by a core restart.".into(),
        CancelReason::Shutdown => "The read was interrupted because the core stopped.".into(),
    }
}

/// One line for what a read is doing, `None` once it holds data (or nothing was asked).
pub(crate) fn phase_line(phase: &Phase) -> Option<&str> {
    match phase {
        Phase::Waiting => Some(LOADING),
        Phase::Failed(text) => Some(text),
        Phase::Idle | Phase::Ready => None,
    }
}

/// What a read shows while it waits for the core.
pub(crate) const LOADING: &str = "Loading…";

/// [`phase_line`] drawn in the legacy panes' style: a muted line while waiting, and a
/// failure in the error colour.
pub(crate) fn phase_label(ui: &mut egui::Ui, phase: &Phase) {
    match phase {
        Phase::Waiting => {
            ui.label(crate::theme::text::muted(LOADING));
        }
        Phase::Failed(text) => {
            crate::theme::widgets::reason_label(ui, crate::theme::tokens::palette().ERROR_FG, text);
        }
        Phase::Idle | Phase::Ready => {}
    }
}

/// The note under a text the core cut for the wire.
pub(crate) fn cut_note(shown: usize, total: u64) -> String {
    format!("Showing the first {shown} of {total} bytes.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use workbench_protocol::wire::Revision;

    fn view(id: &OperationId, state: OperationState) -> OperationView {
        OperationView {
            operation_id: id.clone(),
            state,
            revision: Revision(1),
            result: None,
        }
    }

    #[test]
    fn data_is_taken_once_for_the_current_ask_and_a_pending_answer_never_undoes_it() {
        let mut read = Read::default();
        let id = read.begin("memory");
        let generation = read.generation();
        assert_eq!(
            read.accept(generation, &view(&id, OperationState::Pending), Some(1)),
            None
        );
        assert_eq!(*read.phase(), Phase::Waiting);
        assert_eq!(
            read.accept(generation, &view(&id, OperationState::Ready), Some(7)),
            Some(7)
        );
        assert_eq!(*read.phase(), Phase::Ready);
        assert_eq!(
            read.accept::<u8>(generation, &view(&id, OperationState::Pending), None),
            None
        );
        assert_eq!(*read.phase(), Phase::Ready);
    }

    #[test]
    fn an_answer_of_an_earlier_ask_or_another_operation_is_dropped() {
        let mut read = Read::default();
        let first = read.begin("memory");
        let first_generation = read.generation();
        let second = read.begin("memory");
        assert_ne!(first, second);
        assert_eq!(
            read.accept(
                first_generation,
                &view(&first, OperationState::Ready),
                Some(1)
            ),
            None
        );
        assert_eq!(*read.phase(), Phase::Waiting);
        assert_eq!(
            read.accept(
                read.generation(),
                &view(&first, OperationState::Ready),
                Some(1)
            ),
            None
        );
        let generation = read.generation();
        read.forget();
        assert_eq!(
            read.accept(generation, &view(&second, OperationState::Ready), Some(1)),
            None
        );
        assert_eq!(*read.phase(), Phase::Idle);
    }

    #[test]
    fn a_settling_event_asks_again_once_and_failures_are_shown() {
        let mut read = Read::default();
        let id = read.begin("memory");
        read.event(&view(&id, OperationState::Pending));
        assert!(!read.take_ask_again());
        read.event(&view(&id, OperationState::Ready));
        assert!(read.take_ask_again());
        assert!(!read.take_ask_again());
        read.accept::<u8>(
            read.generation(),
            &view(
                &id,
                OperationState::Failed {
                    code: ErrorCode::NotFound,
                },
            ),
            None,
        );
        assert!(matches!(read.phase(), Phase::Failed(text) if text.contains("Not found")));
        let id = read.begin("memory");
        read.accept::<u8>(
            read.generation(),
            &view(
                &id,
                OperationState::Cancelled {
                    reason: CancelReason::Shutdown,
                },
            ),
            None,
        );
        assert!(phase_line(read.phase()).is_some());
    }
}
