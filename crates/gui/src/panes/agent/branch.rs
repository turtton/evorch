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
    pub threads: &'a [ThreadRecord],
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
        workspace_ui::point_versions(branch.threads, thread, Some(&point))
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = SP_1;
        if action_button(ui, true, icons::GIT_FORK, "ここから fork")
            .on_hover_text(
                "ここから fork: このターンまでの会話を引き継いだ新しい thread を作ります",
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
                "ここまで巻き戻す",
            )
            .on_hover_text(
                "ここまで巻き戻す: このターンの直後まで会話を戻します。今の会話は版として残ります",
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
    let versions = workspace_ui::point_versions(branch.threads, &branch.thread.id, point.as_ref());
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = SP_1;
        // Laid out right to left: the switcher sits at the far right.
        if let Some(current) = version_position(branch, &versions) {
            version_switcher(ui, &versions, current, action);
        }
        let label = "ここから編集";
        let edit = ui
            .add_enabled(
                branch.rewind_block.is_none(),
                frameless((action_icon(icons::PENCIL_SIMPLE), small("編集"))),
            )
            .on_hover_text("ここから編集: このメッセージの直前まで戻し、内容を入力欄に戻します")
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
        let text = "ここから分岐しました";
        let label = ui.label(small(&with_icon(icons::GIT_FORK, text)));
        label.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
        let Some(branch) = branch.filter(|_| own) else {
            return;
        };
        let Some(parent) = branch.thread.parent_thread_id.as_ref() else {
            return;
        };
        let title = branch
            .threads
            .iter()
            .find(|thread| &thread.id == parent)
            .map_or_else(|| parent.to_string(), |thread| thread.title.clone());
        if small_button(
            ui,
            true,
            icons::ARROW_LEFT,
            &format!("分岐元「{title}」を開く"),
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
    let versions = workspace_ui::point_versions(branch.threads, &branch.thread.id, None);
    let Some(current) = version_position(branch, &versions) else {
        return;
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(small("最初のメッセージを編集した版があります"));
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
                if action_button(ui, current > 0, icons::CARET_LEFT, "前の版")
                    .on_hover_text("前の版")
                    .clicked()
                {
                    *action = Some(SidebarAction::SwitchVersion(versions[current - 1].clone()));
                }
            }
            Part::Count => {
                let name = format!("版 {} / {}", current + 1, versions.len());
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
                    "次の版",
                )
                .on_hover_text("次の版")
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
            ui.heading("ここから編集");
            ui.label("このメッセージの直前まで会話を戻し、内容を入力欄に戻します。");
        } else {
            ui.heading("会話を巻き戻す");
            ui.label(format!(
                "ターン {} の直後まで会話を戻します。",
                pending.turn
            ));
        }
        ui.label(format!(
            "巻き戻し前の会話は版として残り、{} {} でいつでも切り替えられます。",
            icons::CARET_LEFT,
            icons::CARET_RIGHT
        ));
        ui.label(
            egui::RichText::new("ファイルの変更は巻き戻りません。").color(palette().WARNING_FG),
        );
        ui.add_space(SP_2);
        ui.horizontal(|ui| {
            if ui.button("キャンセル").clicked() {
                close = true;
            }
            if ui.button("巻き戻す").clicked() {
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
