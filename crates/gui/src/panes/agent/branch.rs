//! Turn actions: fork from a completed turn, rewind to it, edit a turn's first
//! message, and switch between rewound versions.

use workspace_ui::{LineageKind, ThreadId, ThreadRecord};

use crate::model::transcript::TranscriptModel;
use crate::panes::sidebar::SidebarAction;
use crate::theme::icons::{self, with_icon};
use crate::theme::tokens::*;
use crate::theme::widgets::{accessible, ghost};

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

fn small_button(ui: &mut egui::Ui, enabled: bool, icon: &str, label: &str) -> egui::Response {
    let response = ui.add_enabled(enabled, ghost(small(&with_icon(icon, label))));
    accessible(&response, label);
    response
}

/// Fork and rewind actions after a completed turn, plus versions branching there.
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
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = SP_1;
        if small_button(ui, true, icons::GIT_FORK, "ここから fork")
            .on_hover_text("このターンまでの会話を引き継いだ新しい thread を作ります")
            .clicked()
        {
            *action = Some(SidebarAction::ForkAtTurn {
                thread: thread.clone(),
                entry_id,
            });
        }
        let rewind = small_button(
            ui,
            branch.rewind_block.is_none(),
            icons::ARROW_COUNTER_CLOCKWISE,
            "ここまで巻き戻す",
        )
        .on_hover_text("このターンの直後まで会話を戻します。今の会話は版として残ります")
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
        let versions = workspace_ui::branch_versions(branch.threads, thread, Some(&point));
        version_switcher(ui, branch, &versions, action);
    });
}

/// "Edit from here" under the first user message of a turn.
pub(super) fn edit_button(
    ui: &mut egui::Ui,
    model: &TranscriptModel,
    entry_id: usize,
    branch: &BranchContext<'_>,
) {
    if model.edit_point(entry_id).is_none() {
        return;
    }
    ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
        let edit = small_button(
            ui,
            branch.rewind_block.is_none(),
            icons::PENCIL_SIMPLE,
            "ここから編集",
        )
        .on_hover_text("このメッセージの直前まで戻し、内容を入力欄に戻します")
        .on_disabled_hover_text(branch.rewind_block.unwrap_or_default());
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

/// Where the conversation leaves inherited history. Only the thread's own
/// marker offers navigation; inherited markers are plain dividers.
pub(super) fn branch_divider(
    ui: &mut egui::Ui,
    kind: LineageKind,
    own: bool,
    branch: Option<&BranchContext<'_>>,
    action: &mut Option<SidebarAction>,
) {
    ui.add_space(SP_1);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = SP_2;
        let (icon, text) = match kind {
            LineageKind::Fork => (icons::GIT_FORK, "ここから分岐しました"),
            LineageKind::Rewind => (icons::ARROW_COUNTER_CLOCKWISE, "ここで巻き戻しました"),
        };
        let label = ui.label(small(&with_icon(icon, text)));
        label.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
        let Some(branch) = branch.filter(|_| own) else {
            return;
        };
        let Some(parent) = branch.thread.parent_thread_id.as_ref() else {
            return;
        };
        match kind {
            LineageKind::Fork => {
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
            }
            LineageKind::Rewind => {
                let point = branch
                    .thread
                    .lineage
                    .as_ref()
                    .and_then(|lineage| lineage.point.as_ref());
                let versions = workspace_ui::branch_versions(branch.threads, parent, point);
                version_switcher(ui, branch, &versions, action);
                if small_button(ui, true, icons::ARROW_U_UP_LEFT, "巻き戻し前に戻す").clicked()
                {
                    *action = Some(SidebarAction::SwitchVersion(parent.clone()));
                }
            }
        }
    });
    ui.separator();
}

/// Versions rewound to the start of this conversation, shown above its first message.
pub(super) fn start_versions(
    ui: &mut egui::Ui,
    branch: &BranchContext<'_>,
    action: &mut Option<SidebarAction>,
) {
    let versions = workspace_ui::branch_versions(branch.threads, &branch.thread.id, None);
    if versions.len() < 2 {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(small("最初のメッセージを編集した版があります"));
        version_switcher(ui, branch, &versions, action);
    });
}

fn version_switcher(
    ui: &mut egui::Ui,
    branch: &BranchContext<'_>,
    versions: &[ThreadId],
    action: &mut Option<SidebarAction>,
) {
    if versions.len() < 2 {
        return;
    }
    let Some(current) = versions.iter().position(|id| id == &branch.thread.id) else {
        return;
    };
    if small_button(ui, current > 0, icons::CARET_LEFT, "前の版").clicked() {
        *action = Some(SidebarAction::SwitchVersion(versions[current - 1].clone()));
    }
    let text = format!("版 {} / {}", current + 1, versions.len());
    let label = ui.label(small(&text));
    label.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &text));
    if small_button(
        ui,
        current + 1 < versions.len(),
        icons::CARET_RIGHT,
        "次の版",
    )
    .clicked()
    {
        *action = Some(SidebarAction::SwitchVersion(versions[current + 1].clone()));
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
        ui.label("巻き戻し前の会話は版として残り、「巻き戻し前に戻す」でいつでも戻せます。");
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
