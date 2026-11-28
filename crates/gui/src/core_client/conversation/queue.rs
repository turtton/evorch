//! The queue of follow-ups above the composer with its Edit, Restore and Withdraw buttons,
//! and the "Re-attach" hint of a restored input (ADR 0045 E7).
//!
//! Edit and Restore only start an operation in the execution feature
//! (`features/execution/queued_input.rs`); what it finds is applied to the composer here,
//! once per frame ([`settle`]), and only to a composer that still looks as it did when the
//! button was pressed.
use std::collections::BTreeMap;

use egui::{RichText, Ui};
use workbench_protocol::namespaces::ConversationCommand;
use workbench_protocol::{commands::*, ids::*, views::*};

use super::{ConversationIo, ConversationPane};
use crate::core_client::attachments::ClientAttachmentDraft;
use crate::core_client::cache::ReceiptRead;
use crate::core_client::connection::Action;
use crate::core_client::features::execution::queued_input::{
    Guard, Intent, Kind, Poll, Polled, QueueView, Request, Stage,
};
use crate::core_client::shell::Effect;
use crate::core_client::{Draft, Shared};
use crate::theme::icons;
use crate::theme::text::muted;
use crate::theme::tokens::*;
use crate::theme::widgets::ghost;

/// What a restored input carried that the person has to attach again. Attachments are
/// never given back by the core: only how many there were.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reattach {
    pub images: u32,
    pub files: u32,
}

impl Reattach {
    pub fn from_counts(image_count: u32, attachment_count: u32) -> Self {
        Self {
            images: image_count,
            files: attachment_count.saturating_sub(image_count),
        }
    }

    /// What is still missing given the files of the composer: a file of another kind does
    /// not stand in for an image.
    pub fn remaining(&self, selected_images: usize, selected_files: usize) -> Self {
        let left =
            |need: u32, have: usize| need.saturating_sub(u32::try_from(have).unwrap_or(u32::MAX));
        Self {
            images: left(self.images, selected_images),
            files: left(self.files, selected_files),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.images == 0 && self.files == 0
    }
}

fn is_image(file: &ClientAttachmentDraft) -> bool {
    file.file.media_type.starts_with("image/")
}

fn plural(count: u32, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// How the composer of `thread` looks: what an Edit or Restore compares against.
pub(in crate::core_client) fn guard_of(
    thread: &ThreadId,
    drafts: &BTreeMap<String, Draft>,
    files: &BTreeMap<String, Vec<ClientAttachmentDraft>>,
    loading: &BTreeMap<String, usize>,
) -> Guard {
    Guard {
        thread: thread.clone(),
        draft_revision: drafts
            .get(thread.as_str())
            .map_or(0, |draft| draft.revision),
        files: files
            .get(thread.as_str())
            .into_iter()
            .flatten()
            .map(|file| file.id.clone())
            .collect(),
        loading: loading.get(thread.as_str()).copied().unwrap_or(0),
    }
}

/// Edit and Restore replace the composer, so they wait for an empty one with nothing being
/// sent or read.
fn composer_is_free(io: &ConversationIo<'_>, thread: &ThreadId) -> bool {
    let text_free = io
        .drafts
        .get(thread.as_str())
        .is_none_or(|draft| draft.text.trim().is_empty());
    let guard = guard_of(
        thread,
        io.drafts,
        io.attachment_drafts,
        io.loading_attachments,
    );
    text_free
        && guard.files.is_empty()
        && guard.loading == 0
        && !io.sending.contains(thread.as_str())
}

/// The "Re-attach N images" line under the attachments of the composer.
pub(super) fn reattach_hint(ui: &mut Ui, io: &ConversationIo<'_>, thread: &ThreadId) {
    let Some(need) = io
        .drafts
        .get(thread.as_str())
        .map(|draft| &draft.reattach)
        .filter(|need| !need.is_empty())
    else {
        return;
    };
    let files = io
        .attachment_drafts
        .get(thread.as_str())
        .map(Vec::as_slice)
        .unwrap_or_default();
    let images = files.iter().filter(|file| is_image(file)).count();
    let left = need.remaining(images, files.len() - images);
    for text in [
        (left.images > 0).then(|| format!("Re-attach {}", plural(left.images, "image", "images"))),
        (left.files > 0).then(|| format!("Re-attach {}", plural(left.files, "file", "files"))),
    ]
    .into_iter()
    .flatten()
    {
        ui.label(muted(text))
            .on_hover_text("Pictures and files are not kept with a queued input. Attach them again with +, paste or drop.");
    }
}

/// The rows of the queue, the failure of the last Edit or Restore and a failed read of the
/// receipts. Returns what was pressed.
pub(super) fn queue_rows(
    ui: &mut Ui,
    io: &ConversationIo<'_>,
    receipts: &[&InputReceipt],
    thread: &ThreadId,
    view: &QueueView,
) -> Option<Effect> {
    let free = composer_is_free(io, thread) && view.busy.is_none() && io.connected;
    let guard = guard_of(
        thread,
        io.drafts,
        io.attachment_drafts,
        io.loading_attachments,
    );
    let mut pressed = None;
    if let Some(ReceiptRead::Failed(reason)) = io.cache.receipt_read(thread.as_str()) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("Could not read the queued inputs: {reason}"))
                    .size(FONT_SMALL)
                    .color(palette().WARNING_FG),
            );
            if ui
                .add_enabled(io.connected, ghost("Retry"))
                .on_hover_text("Read the queued inputs again")
                .clicked()
            {
                pressed = Some(Effect::send(Action::Receipts(thread.clone())));
            }
        });
    }
    for receipt in receipts {
        ui.vertical(|ui| {
            if let Some(effect) = row(ui, receipt, thread, view, &guard, free, io.connected) {
                pressed = Some(effect);
            }
            if let Some(preview) = &receipt.preview {
                let line = preview.text.lines().find(|line| !line.trim().is_empty());
                let mut text = line.unwrap_or("").to_owned();
                if preview.truncated
                    || preview
                        .text
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .count()
                        > 1
                {
                    text.push('…');
                }
                let attachments = (preview.attachment_count > 0).then(|| {
                    format!(
                        " [{}]",
                        plural(preview.attachment_count, "attachment", "attachments")
                    )
                });
                ui.add(
                    egui::Label::new(muted(format!("{text}{}", attachments.unwrap_or_default())))
                        .truncate(),
                );
            }
        });
    }
    if let Some(failure) = view.failure.as_ref().filter(|f| &f.thread == thread) {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(&failure.message)
                    .size(FONT_SMALL)
                    .color(palette().WARNING_FG),
            );
            if ui.add(ghost("Dismiss")).clicked() {
                pressed = Some(Effect::Queue(Intent::Dismiss));
            }
        });
    }
    pressed
}

fn row(
    ui: &mut Ui,
    receipt: &InputReceipt,
    thread: &ThreadId,
    view: &QueueView,
    guard: &Guard,
    free: bool,
    connected: bool,
) -> Option<Effect> {
    let mut pressed = None;
    let busy = view
        .busy
        .as_ref()
        .filter(|busy| busy.submission == receipt.submission_id);
    ui.horizontal(|ui| {
        let (icon, text, color) = match receipt.state {
            InputReceiptState::Accepted => (icons::QUEUE, "Queued follow-up", palette().TEXT_MUTED),
            InputReceiptState::Dropped => {
                (icons::WARNING, "Follow-up dropped", palette().WARNING_FG)
            }
            _ => (
                icons::QUESTION,
                "Follow-up state unknown",
                palette().WARNING_FG,
            ),
        };
        ui.label(
            RichText::new(icons::with_icon(icon, text))
                .size(FONT_SMALL)
                .color(color),
        )
        .on_hover_text(receipt.submission_id.as_str());
        if let Some(busy) = busy {
            ui.label(muted(match busy.stage {
                Stage::Reading => "Reading…",
                Stage::TakingBack => "Taking it back…",
            }));
            return;
        }
        let editable = receipt.preview.is_some() && receipt.run.is_some();
        let start = |kind| {
            receipt.run.as_ref().map(|run| {
                Effect::Queue(Intent::Start(Request {
                    kind,
                    thread: thread.clone(),
                    submission: receipt.submission_id.clone(),
                    run: run.clone(),
                    guard: guard.clone(),
                }))
            })
        };
        if receipt.state == InputReceiptState::Accepted
            && let Some(run) = &receipt.run
        {
            if ui.add_enabled(connected, ghost("Withdraw")).clicked() {
                pressed = Some(Effect::send(super::command(Command::Conversation(
                    ConversationCommand::WithdrawFollowUp(WithdrawFollowUp {
                        thread: thread.clone(),
                        run: run.clone(),
                        submission_id: receipt.submission_id.clone(),
                    }),
                ))));
            }
            if edit_button(ui, "Edit", editable && free, editable, "Take this follow-up back and put its text in the composer") {
                pressed = start(Kind::Edit);
            }
        } else if receipt.state != InputReceiptState::Accepted {
            let hover = if receipt.state == InputReceiptState::Unknown {
                "Copy the text to the composer. It may already have reached the agent: sending it again can duplicate it."
            } else {
                "Copy the text to the composer"
            };
            if edit_button(ui, "Restore", editable && free, editable, hover) {
                pressed = start(Kind::Restore);
            }
        }
    });
    pressed
}

/// A button that says why it is off: the words were not kept, or the composer is in use.
fn edit_button(ui: &mut Ui, label: &str, enabled: bool, has_words: bool, hover: &str) -> bool {
    let off = if has_words {
        "Clear the composer first: it is replaced by the text. Wait for any other Edit or Restore to finish."
    } else {
        "The text of this input was not kept"
    };
    ui.add_enabled(enabled, ghost(label))
        .on_hover_text(hover)
        .on_disabled_hover_text(off)
        .clicked()
}

/// Once per frame, after the effects: gives the operation the composer's look, and puts what
/// it found in the composer. Returns what the operation wants of the connection.
pub(in crate::core_client) fn settle(shared: &mut Shared, pane: &mut ConversationPane) -> Needs {
    let (drafts, files, loading) = (
        &shared.drafts,
        &shared.attachment_drafts,
        &shared.loading_attachments,
    );
    let Some(execution) = shared.features.execution_mut() else {
        return Needs::default();
    };
    let Polled {
        poll,
        wants_pass,
        receipts,
    } = execution.queue_poll(|thread| guard_of(thread, drafts, files, loading));
    match poll {
        Poll::Nothing => {}
        Poll::Apply { request, words } => {
            let draft = shared.drafts.entry(request.thread.to_string()).or_default();
            draft.text = words.text;
            draft.revision += 1;
            draft.reattach = Reattach::from_counts(words.image_count, words.attachment_count);
            shared.status = match request.kind {
                Kind::Edit => "The follow-up was taken back and is in the composer.".into(),
                Kind::Restore => "The text is in the composer. It was not sent.".into(),
            };
            pane.request_composer_focus();
        }
        Poll::TakenBackButChanged { request } => {
            pane.notice(
                &request.thread,
                "A follow-up was taken back, but the composer had changed, so its text was not put there and is not kept."
                    .into(),
            );
            shared.status = "The follow-up was taken back; the composer had changed.".into();
        }
    }
    Needs {
        repaint: wants_pass,
        receipts,
    }
}

/// What the operation asks of the frame after [`settle`].
#[derive(Default)]
pub(in crate::core_client) struct Needs {
    /// The feature has a request to send in its next pass.
    pub repaint: bool,
    /// The thread whose receipts the connection is to read, once.
    pub receipts: Option<ThreadId>,
}
