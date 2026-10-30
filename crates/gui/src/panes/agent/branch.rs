//! Turn actions: fork from a completed turn, edit a turn's first message, and
//! switch between the versions an edit leaves. Editing a message rewinds to the
//! turn before it, so a plain rewind is offered only where no editable message
//! starts the next turn (e.g. a turn started by a goal or a child run).

use workspace_ui::{LineageKind, ThreadId, ThreadRecord};

use crate::model::transcript::TranscriptModel;
use crate::panes::sidebar::SidebarAction;
use crate::theme::icons::{self, with_icon};
use crate::theme::tokens::*;
use crate::theme::widgets::{accessible, ghost, icon_text};

/// The active thread's branching state, present only for the thread conversation.
pub struct BranchContext<'a> {
    pub thread: &'a ThreadRecord,
    /// Version groups of the sidebar's threads, built once for this frame.
    pub lineage: &'a workspace_ui::LineageIndex<'a>,
    /// Why rewinds are unavailable now; forks stay available.
    pub rewind_block: Option<&'static str>,
}

#[derive(Clone)]
struct PendingRewind {
    thread: ThreadId,
    entry_id: usize,
    edit: bool,
    turn: usize,
}

fn pending_id() -> egui::Id {
    egui::Id::new("conversation-rewind-confirm")
}

fn small(text: &str) -> egui::RichText {
    egui::RichText::new(text)
        .size(FONT_SMALL)
        .color(palette().TEXT_MUTED)
}

/// Side of a turn action's hit area, matching row icon buttons.
const ACTION_SIDE: f32 = ROW_DENSE - SP_1;

fn action_icon(icon: &str) -> egui::RichText {
    icon_text(icon).color(palette().TEXT_MUTED)
}

fn frameless<'a>(atoms: impl egui::IntoAtoms<'a>) -> egui::Button<'a> {
    egui::Button::new(atoms)
        .frame_when_inactive(false)
        .corner_radius(egui::CornerRadius::same(R_SM))
        .min_size(egui::vec2(ACTION_SIDE, ACTION_SIDE))
}

fn small_button(ui: &mut egui::Ui, enabled: bool, icon: &str, label: &str) -> egui::Response {
    let response = ui.add_enabled(enabled, ghost(small(&with_icon(icon, label))));
    accessible(&response, label);
    response
}

/// Icon-only turn action; callers add the hover text.
fn action_button(ui: &mut egui::Ui, enabled: bool, icon: &str, label: &str) -> egui::Response {
    let response = ui.add_enabled(enabled, frameless(action_icon(icon)));
    accessible(&response, label);
    response
}

/// Fork after a completed turn. When no editable message starts the next turn,
/// the rewind and the versions after this turn are offered here too.
pub(super) fn turn_footer(
    ui: &mut egui::Ui,
    model: &TranscriptModel,
    entry_id: usize,
    branch: &BranchContext<'_>,
    action: &mut Option<SidebarAction>,
) {
    let Some(point) = model.fork_point(entry_id) else {
        return;
    };
    let thread = &branch.thread.id;
    let edit_follows = model.edit_after_turn(entry_id).is_some();
    let versions = if edit_follows {
        Vec::new()
    } else {
        branch.lineage.point_versions(thread, Some(&point))
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = SP_1;
        if action_button(ui, true, icons::GIT_FORK, "Fork from here")
            .on_hover_text(
                "Fork from here: start a new thread that carries the conversation up to this turn",
            )
            .clicked()
        {
            *action = Some(SidebarAction::ForkAtTurn {
                thread: thread.clone(),
                entry_id,
            });
        }
        if !edit_follows && model.continues_after(entry_id) {
            let rewind = action_button(
                ui,
                branch.rewind_block.is_none(),
                icons::ARROW_COUNTER_CLOCKWISE,
                "Rewind to here",
            )
            .on_hover_text(
                "Rewind to here: return to just after this turn. The current conversation stays as a version",
            )
            .on_disabled_hover_text(branch.rewind_block.unwrap_or_default());
            if rewind.clicked() {
                request_confirm(
                    ui,
                    PendingRewind {
                        thread: thread.clone(),
                        entry_id,
                        edit: false,
                        turn: model.turn_number(entry_id) + 1,
                    },
                );
            }
        }
        if let Some(current) = version_position(branch, &versions) {
            ui.separator();
            version_switcher(ui, &versions, current, action);
        }
    });
}

/// "Edit from here" under the first user message of a turn, with the versions
/// earlier edits of it left.
pub(super) fn edit_row(
    ui: &mut egui::Ui,
    model: &TranscriptModel,
    entry_id: usize,
    branch: &BranchContext<'_>,
    action: &mut Option<SidebarAction>,
) {
    let Some(point) = model.edit_point(entry_id) else {
        return;
    };
    let versions = branch
        .lineage
        .point_versions(&branch.thread.id, point.as_ref());
    // One row tall: a centered horizontal layout grows each widget to the
    // height it is given, and `with_layout` would give it the rest of the
    // scroll viewport, pushing the transcript below the first message down.
    let row = egui::vec2(ui.available_width(), ACTION_SIDE);
    let layout = egui::Layout::right_to_left(egui::Align::Center);
    ui.allocate_ui_with_layout(row, layout, |ui| {
        ui.spacing_mut().item_spacing.x = SP_1;
        // Laid out right to left: the switcher sits at the far right.
        if let Some(current) = version_position(branch, &versions) {
            version_switcher(ui, &versions, current, action);
        }
        let label = "Edit from here";
        let edit = ui
            .add_enabled(
                branch.rewind_block.is_none(),
                frameless((action_icon(icons::PENCIL_SIMPLE), small("Edit"))),
            )
            .on_hover_text("Edit from here: return to just before this message and put its content back in the input")
            .on_disabled_hover_text(branch.rewind_block.unwrap_or_default());
        accessible(&edit, label);
        if edit.clicked() {
            request_confirm(
                ui,
                PendingRewind {
                    thread: branch.thread.id.clone(),
                    entry_id,
                    edit: true,
                    turn: model.turn_number(entry_id),
                },
            );
        }
    });
}

/// Where a fork leaves inherited history. Only the thread's own marker offers
/// navigation; inherited markers are plain dividers. Rewinds draw nothing: their
/// versions switch at the edited message or the turn before it.
pub(super) fn branch_divider(
    ui: &mut egui::Ui,
    kind: LineageKind,
    own: bool,
    branch: Option<&BranchContext<'_>>,
    action: &mut Option<SidebarAction>,
) {
    if kind == LineageKind::Rewind {
        return;
    }
    ui.add_space(SP_1);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = SP_2;
        let text = "Branched from here";
        let label = ui.label(small(&with_icon(icons::GIT_FORK, text)));
        label.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
        let Some(branch) = branch.filter(|_| own) else {
            return;
        };
        let Some(parent) = branch.thread.parent_thread_id.as_ref() else {
            return;
        };
        let title = branch
            .lineage
            .get(parent)
            .map_or_else(|| parent.to_string(), |thread| thread.title.clone());
        if small_button(
            ui,
            true,
            icons::ARROW_LEFT,
            &format!("Open source thread \"{title}\""),
        )
        .clicked()
        {
            *action = Some(SidebarAction::SwitchThread(parent.clone()));
        }
    });
    ui.separator();
}

/// Versions rewound to the start of this conversation, shown above it when no
/// first message offers them (e.g. right after editing the first message).
pub(super) fn start_versions(
    ui: &mut egui::Ui,
    model: &TranscriptModel,
    branch: &BranchContext<'_>,
    action: &mut Option<SidebarAction>,
) {
    if model.start_edit_message().is_some() {
        return;
    }
    let versions = branch.lineage.point_versions(&branch.thread.id, None);
    let Some(current) = version_position(branch, &versions) else {
        return;
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(small("The first message has edited versions"));
        version_switcher(ui, &versions, current, action);
    });
}

/// The active thread's place among `versions`, when there is more than one.
fn version_position(branch: &BranchContext<'_>, versions: &[ThreadId]) -> Option<usize> {
    if versions.len() < 2 {
        return None;
    }
    versions.iter().position(|id| id == &branch.thread.id)
}

/// `‹ n/m ›`, in reading order whichever way the enclosing layout runs.
fn version_switcher(
    ui: &mut egui::Ui,
    versions: &[ThreadId],
    current: usize,
    action: &mut Option<SidebarAction>,
) {
    #[derive(Clone, Copy)]
    enum Part {
        Previous,
        Count,
        Next,
    }
    let order = if ui.layout().prefer_right_to_left() {
        [Part::Next, Part::Count, Part::Previous]
    } else {
        [Part::Previous, Part::Count, Part::Next]
    };
    for part in order {
        match part {
            Part::Previous => {
                if action_button(ui, current > 0, icons::CARET_LEFT, "Previous version")
                    .on_hover_text("Previous version")
                    .clicked()
                {
                    *action = Some(SidebarAction::SwitchVersion(versions[current - 1].clone()));
                }
            }
            Part::Count => {
                let name = format!("Version {} / {}", current + 1, versions.len());
                let label = ui.label(small(&format!("{}/{}", current + 1, versions.len())));
                label.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &name)
                });
            }
            Part::Next => {
                if action_button(
                    ui,
                    current + 1 < versions.len(),
                    icons::CARET_RIGHT,
                    "Next version",
                )
                .on_hover_text("Next version")
                .clicked()
                {
                    *action = Some(SidebarAction::SwitchVersion(versions[current + 1].clone()));
                }
            }
        }
    }
}

fn request_confirm(ui: &egui::Ui, pending: PendingRewind) {
    ui.ctx()
        .data_mut(|data| data.insert_temp(pending_id(), Some(pending)));
}

/// Confirmation for rewinds. Nothing is deleted, so confirm is not destructive.
pub(super) fn confirm_modal(ui: &egui::Ui, action: &mut Option<SidebarAction>) {
    let ctx = ui.ctx().clone();
    let Some(pending) = ctx
        .data(|data| data.get_temp::<Option<PendingRewind>>(pending_id()))
        .flatten()
    else {
        return;
    };
    let mut close = false;
    let modal = egui::Modal::new(egui::Id::new("conversation-rewind-modal")).show(&ctx, |ui| {
        ui.set_max_width(420.0);
        if pending.edit {
            ui.heading("Edit from here");
            ui.label("Return to just before this message and put its content back in the input.");
        } else {
            ui.heading("Rewind the conversation");
            ui.label(format!("Return to just after turn {}.", pending.turn));
        }
        ui.label(format!(
            "The conversation before the rewind stays as a version; switch any time with {} {}.",
            icons::CARET_LEFT,
            icons::CARET_RIGHT
        ));
        ui.label(egui::RichText::new("File changes are not rewound.").color(palette().WARNING_FG));
        ui.add_space(SP_2);
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                close = true;
            }
            if ui.button("Rewind").clicked() {
                *action = Some(if pending.edit {
                    SidebarAction::EditFromMessage {
                        thread: pending.thread.clone(),
                        entry_id: pending.entry_id,
                    }
                } else {
                    SidebarAction::RewindToTurn {
                        thread: pending.thread.clone(),
                        entry_id: pending.entry_id,
                    }
                });
                close = true;
            }
        });
    });
    if close || modal.should_close() {
        ctx.data_mut(|data| data.remove::<Option<PendingRewind>>(pending_id()));
    }
}
