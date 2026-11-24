//! The "Link PR…" dialog and the confirmation of moving auto fix, as the embedded workbench
//! draws them. Both are modals the Pull requests tab draws.
//!
//! Unlike the embedded workbench, the "Link PR…" dialog stays open while its operation runs:
//! the core, not this screen, reads the locator, and its refusal is said in the dialog.
use workbench_protocol::ids::{OperationId, ThreadId};
use workbench_protocol::namespaces::pull_requests::{
    ChangeAutoFixDestination, LinkPullRequest, PullRequestsCommand, ResetAutoFixBudget,
};
use workbench_protocol::wire::Revision;

use super::{FeatureContext, PullRequestsFeature, new_operation};
use crate::panes::pull_request_widgets::{
    AutoFixControls, DestinationDialogAction, LinkDialogAction, Reading, destination_dialog_view,
    link_dialog_view,
};
use crate::theme::tokens::palette;
use egui::Color32;

pub(super) struct LinkDialog {
    pub thread: ThreadId,
    pub input: String,
    pub operation: Option<OperationId>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Change {
    TurnOn,
    ResetBudget,
}

/// Taking the notices over from another thread moves them away from it, so the person
/// confirms the exact thread they leave before the command is sent.
pub(super) struct DestinationMove {
    pub thread: ThreadId,
    pub change: Change,
    pub previous: ThreadId,
    pub link_revision: Revision,
    pub policy_revision: Revision,
}

impl PullRequestsFeature {
    pub(super) fn open_link(&mut self, thread: ThreadId) {
        self.want(thread.clone());
        // A read that failed is tried again when the dialog is opened again.
        if let Some(entry) = self.threads.get_mut(&thread)
            && entry.error.is_some()
            && !entry.in_flight
        {
            entry.stale = true;
        }
        self.link = Some(LinkDialog {
            thread,
            input: String::new(),
            operation: None,
            note: None,
        });
    }

    pub(super) fn ask_destination(
        &mut self,
        thread: &ThreadId,
        previous: &str,
        controls: &AutoFixControls,
        change: Change,
    ) {
        let Ok(previous) = ThreadId::new(previous) else {
            return;
        };
        self.destination = Some(DestinationMove {
            thread: thread.clone(),
            change,
            previous,
            link_revision: Revision(controls.link_revision),
            policy_revision: Revision(controls.policy_revision),
        });
    }

    pub(super) fn draw_destination(&mut self, egui: &egui::Context, ctx: &mut FeatureContext<'_>) {
        let Some(change) = &self.destination else {
            return;
        };
        let previous_label = ctx
            .cache()
            .threads
            .get(change.previous.as_str())
            .map_or_else(
                || change.previous.to_string(),
                |thread| thread.title.clone(),
            );
        match destination_dialog_view(egui, &previous_label) {
            Some(DestinationDialogAction::Move) => {
                let Some(change) = self.destination.take() else {
                    return;
                };
                if self.running() {
                    return;
                }
                let confirmed_previous = Some(change.previous);
                let command = match change.change {
                    Change::TurnOn => {
                        PullRequestsCommand::ChangeAutoFixDestination(ChangeAutoFixDestination {
                            operation_id: new_operation(),
                            thread: change.thread,
                            link_revision: change.link_revision,
                            policy_revision: change.policy_revision,
                            confirmed_previous,
                        })
                    }
                    Change::ResetBudget => {
                        PullRequestsCommand::ResetAutoFixBudget(ResetAutoFixBudget {
                            operation_id: new_operation(),
                            thread: change.thread,
                            link_revision: change.link_revision,
                            policy_revision: change.policy_revision,
                            confirmed_previous,
                        })
                    }
                };
                self.send(ctx, command);
            }
            Some(DestinationDialogAction::Cancel) => self.destination = None,
            None => {}
        }
    }

    /// Whether Link may be pressed; if not, why, and in which colour it is said.
    fn link_readiness(&self, dialog: &LinkDialog) -> Result<(), (String, Color32)> {
        let muted = |why: &str| Err((why.to_owned(), palette().TEXT_MUTED));
        if let Some(error) = self
            .threads
            .get(&dialog.thread)
            .and_then(|entry| entry.error.as_ref())
        {
            return Err((error.clone(), palette().ERROR_FG));
        }
        let Some(view) = self.view_of(&dialog.thread) else {
            return muted("Reading the thread's pull request…");
        };
        if !view.tracking_enabled {
            return muted("Pull request tracking is off");
        }
        if !view.owned_here {
            return muted("Not tracked in this window");
        }
        if dialog
            .operation
            .as_ref()
            .is_some_and(|id| self.operation_running(id))
        {
            return muted("Working…");
        }
        Ok(())
    }

    pub(super) fn draw_link(&mut self, egui: &egui::Context, ctx: &mut FeatureContext<'_>) {
        let Some(dialog) = &self.link else {
            return;
        };
        let readiness = self.link_readiness(dialog);
        let revision = self
            .view_of(&dialog.thread)
            .map(|view| view.lifecycle_revision);
        let Some(dialog) = &mut self.link else {
            return;
        };
        let reading = Reading::unchecked(&dialog.input);
        let ready = readiness.is_ok();
        let note = match readiness {
            Err(why) => Some(why),
            Ok(()) => dialog.note.clone().map(|note| (note, palette().ERROR_FG)),
        };
        let action = link_dialog_view(
            egui,
            &mut dialog.input,
            &reading,
            ready,
            note.as_ref().map(|(text, color)| (text.as_str(), *color)),
        );
        match action {
            Some(LinkDialogAction::Link(locator)) => {
                let (Some(expected_revision), true) = (revision, ready) else {
                    return;
                };
                let thread = dialog.thread.clone();
                dialog.note = None;
                let id = self.send(
                    ctx,
                    PullRequestsCommand::LinkPullRequest(LinkPullRequest {
                        operation_id: new_operation(),
                        thread,
                        locator,
                        expected_revision,
                    }),
                );
                if let Some(dialog) = &mut self.link {
                    dialog.operation = Some(id);
                }
            }
            Some(LinkDialogAction::Close) => self.link = None,
            None => {}
        }
    }
}
