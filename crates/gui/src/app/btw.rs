//! `/btw` side questions: a dismissible window outside the conversation.
//!
//! Nothing here is persisted or fed back. The question and its answer live in this
//! window only: they never enter the transcript, saved history, search index, thread
//! title or the next provider input. One window serves one question at a time.
use std::sync::mpsc::{Receiver, TryRecvError};

use runtime::btw::{BtwError, BtwSource};

use super::WorkbenchState;
use crate::model::commands::{BtwCancel, BtwOutcome, BtwStartError};
use crate::model::tasks::AgentRunSource;

pub const BTW_USAGE: &str = "usage: /btw <question>";

/// Where a side question stands.
#[derive(Clone, PartialEq, Eq)]
pub enum BtwStatus {
    Running,
    Answered { text: String },
    Failed { message: String },
    Cancelled,
}

impl std::fmt::Debug for BtwStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Running => "Running",
            Self::Answered { .. } => "Answered",
            Self::Failed { .. } => "Failed",
            Self::Cancelled => "Cancelled",
        })
    }
}

/// What the side question window shows.
#[derive(Clone, PartialEq, Eq)]
pub struct BtwView {
    pub request_id: u64,
    pub thread_id: String,
    pub question: String,
    pub source: BtwSource,
    pub status: BtwStatus,
}

impl std::fmt::Debug for BtwView {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BtwView")
            .field("request_id", &self.request_id)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

pub(super) struct BtwWindow {
    view: BtwView,
    receiver: Option<Receiver<BtwOutcome>>,
    cancel: Option<BtwCancel>,
}

/// The words for a side question that could not start. They never repeat the question.
fn start_error_text(error: &BtwStartError) -> String {
    match error {
        BtwStartError::Unavailable => "Side questions are unavailable in this window".into(),
        BtwStartError::Refused(error) => refusal_text(error),
    }
}

fn refusal_text(error: &BtwError) -> String {
    match error {
        BtwError::NoLocalContext => "/btw needs this process to hold the conversation: a saved \
            history alone, or a thread another process owns, cannot be asked about"
            .into(),
        BtwError::ContextNotReady => {
            "/btw has nothing to ask about yet: the conversation has not sent a request".into()
        }
        other => format!("/btw failed: {other}"),
    }
}

impl<S: AgentRunSource> WorkbenchState<S> {
    /// The window's content, if one is open.
    pub fn btw_view(&self) -> Option<&BtwView> {
        self.btw.as_ref().map(|window| &window.view)
    }

    pub fn btw_running(&self) -> bool {
        self.btw
            .as_ref()
            .is_some_and(|window| window.view.status == BtwStatus::Running)
    }

    /// Runs `/btw <question>`. Returns whether it started; the composer keeps its
    /// text otherwise.
    pub(super) fn submit_btw(&mut self, question: &str) -> bool {
        if question.is_empty() {
            self.push_notice(BTW_USAGE);
            return false;
        }
        if !self.composer.attachments.is_empty() {
            self.push_notice("/btw does not take image attachments yet");
            return false;
        }
        let Some(thread_id) = self.sidebar.active_thread.as_ref().map(ToString::to_string) else {
            self.push_notice("Select or start a thread first");
            return false;
        };
        if self.btw_running() {
            self.push_notice("A side question is already running — wait for it or dismiss it");
            return false;
        }
        match self.sink.start_btw(&thread_id, question.to_owned()) {
            Ok(job) => {
                self.dismiss_btw();
                self.btw_next_id += 1;
                self.btw = Some(BtwWindow {
                    view: BtwView {
                        request_id: self.btw_next_id,
                        thread_id,
                        question: question.to_owned(),
                        source: job.source,
                        status: BtwStatus::Running,
                    },
                    receiver: Some(job.receiver),
                    cancel: Some(job.cancel),
                });
                true
            }
            Err(error) => {
                self.push_notice(start_error_text(&error));
                false
            }
        }
    }

    /// Cancels a running side question and closes the window.
    pub fn dismiss_btw(&mut self) {
        if let Some(window) = self.btw.take()
            && let Some(cancel) = window.cancel
        {
            cancel.cancel();
        }
    }

    /// Stops a running question but keeps its window to say so.
    pub fn cancel_btw(&mut self) {
        let Some(window) = self.btw.as_mut() else {
            return;
        };
        if window.view.status != BtwStatus::Running {
            return;
        }
        if let Some(cancel) = window.cancel.take() {
            cancel.cancel();
        }
        window.receiver = None;
        window.view.status = BtwStatus::Cancelled;
    }

    /// Takes a finished answer in, and drops the window of a thread that is gone
    /// or no longer shown. A result arriving for a dropped window has nowhere to go.
    pub fn poll_btw(&mut self) -> bool {
        let Some(window) = self.btw.as_ref() else {
            return false;
        };
        let shown = self.sidebar.active_thread.as_ref().map(ToString::to_string);
        let exists = self
            .sidebar
            .threads
            .iter()
            .any(|thread| thread.id.to_string() == window.view.thread_id);
        if shown.as_deref() != Some(window.view.thread_id.as_str()) || !exists {
            self.dismiss_btw();
            return true;
        }
        let Some(window) = self.btw.as_mut() else {
            return false;
        };
        let Some(receiver) = window.receiver.as_ref() else {
            return false;
        };
        let status = match receiver.try_recv() {
            Ok(Ok(answer)) => {
                window.view.source = answer.source;
                BtwStatus::Answered { text: answer.text }
            }
            Ok(Err(BtwError::EmptyAnswer)) => BtwStatus::Failed {
                message: "The model returned no answer".into(),
            },
            Ok(Err(error)) => BtwStatus::Failed {
                message: refusal_text(&error),
            },
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => BtwStatus::Failed {
                message: "The side question ended without an answer".into(),
            },
        };
        window.receiver = None;
        window.cancel = None;
        window.view.status = status;
        true
    }

    /// Escape closes the side question window first, so it never also stops the run.
    pub(super) fn consume_btw_escape(&mut self, ctx: &egui::Context) {
        if self.btw.is_none() {
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.dismiss_btw();
        }
    }

    pub(super) fn render_btw_window(&mut self, ctx: &egui::Context) {
        let Some(window) = self.btw.as_ref() else {
            return;
        };
        let view = window.view.clone();
        let mut open = true;
        let mut cancel = false;
        let mut dismiss = false;
        egui::Window::new("Side question")
            .id(egui::Id::new("btw-window"))
            .collapsible(false)
            .default_width(420.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(format!("/btw {}", view.question)).weak())
                        .wrap(),
                );
                ui.separator();
                match &view.status {
                    BtwStatus::Running => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Thinking…");
                            cancel = ui.button("Cancel").clicked();
                        });
                    }
                    BtwStatus::Answered { text } => {
                        egui::ScrollArea::vertical()
                            .max_height(320.0)
                            .show(ui, |ui| {
                                ui.add(egui::Label::new(text.as_str()).wrap().selectable(true));
                            });
                    }
                    BtwStatus::Failed { message } => {
                        ui.add(egui::Label::new(message.as_str()).wrap());
                    }
                    BtwStatus::Cancelled => {
                        ui.label("Cancelled");
                    }
                }
                ui.separator();
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(if view.source.settled {
                            "Not saved to the conversation."
                        } else {
                            "Not saved to the conversation. Answered from the latest request \
                             sent to the model; its newest step may be missing."
                        })
                        .small()
                        .weak(),
                    )
                    .wrap(),
                );
                if view.status != BtwStatus::Running {
                    dismiss = ui.button("Dismiss").clicked();
                }
            });
        if cancel {
            self.cancel_btw();
        }
        if dismiss || !open {
            self.dismiss_btw();
        }
    }
}
