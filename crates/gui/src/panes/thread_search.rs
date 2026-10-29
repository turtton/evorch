//! The thread search modal opened with Ctrl+K or the sidebar search button.

use std::path::Path;

use egui::RichText;
use workspace_ui::{SidebarState, ThreadId};

use crate::model::thread_search::{ResultRow, ThreadSearchModal};
use crate::theme::icons;
use crate::theme::text::{h3, muted};
use crate::theme::tokens::*;
use crate::theme::widgets::surface_frame;

pub const QUERY_HINT: &str = "Search threads by title and conversation";
pub const SEARCHING_LABEL: &str = "Searching…";
pub const NO_MATCHES_LABEL: &str = "No matching threads";
pub const TITLE_SCAN_LIMITED_LABEL: &str =
    "Fuzzy title matching covered only the newest threads; older titles match exact words only.";
pub const REFINE_LABEL: &str = "Showing the first matches. Refine the query to narrow them.";
const RESULT_MAX_HEIGHT: f32 = 360.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadSearchAction {
    Open(ThreadId),
    Close,
}

pub fn thread_search_modal(
    ctx: &egui::Context,
    modal: &mut ThreadSearchModal,
    sidebar: &SidebarState,
    project: Option<&Path>,
) -> Option<ThreadSearchAction> {
    let mut action = None;
    let rows = modal.rows(sidebar);
    let keys = modal.take_keys();
    if keys.down {
        modal.move_selection(rows.len(), 1);
    }
    if keys.up {
        modal.move_selection(rows.len(), -1);
    }
    let moved = keys.down || keys.up;
    if keys.escape {
        action = Some(ThreadSearchAction::Close);
    }
    let focus = modal.take_focus_request();
    let shown = egui::Modal::new(egui::Id::new("thread-search"))
        .backdrop_color(palette().OVERLAY)
        .frame(surface_frame(palette().SURFACE_RAISED))
        .show(ctx, |ui| {
            ui.set_width(
                (ctx.viewport_rect().width() * 0.6).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 4.0,
            );
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            ui.label(h3("Search threads"));
            let query = ui.add(
                egui::TextEdit::singleline(&mut modal.query)
                    .hint_text(QUERY_HINT)
                    .desired_width(f32::INFINITY),
            );
            if focus {
                query.request_focus();
            }
            if query.changed() {
                modal.reset_selection();
            }
            // Decided after this frame's edit: an Enter that arrives with a
            // keystroke belongs to the text as it is now, and the results shown
            // answer an earlier query.
            let confirmable = !modal.is_stale(project);
            if keys.enter
                && confirmable
                && action.is_none()
                && let Some(row) = rows.get(modal.selected(rows.len()))
            {
                action = Some(ThreadSearchAction::Open(row.thread.clone()));
            }
            status_line(ui, modal, rows.len(), project);
            let opened = ui
                .add_enabled_ui(confirmable, |ui| result_list(ui, modal, &rows, moved))
                .inner;
            if let Some(opened) = opened {
                action = Some(ThreadSearchAction::Open(opened));
            }
        });
    modal.note_layer(shown.response.layer_id);
    if action.is_none() && shown.should_close() {
        action = Some(ThreadSearchAction::Close);
    }
    action
}

fn status_line(ui: &mut egui::Ui, modal: &ThreadSearchModal, rows: usize, project: Option<&Path>) {
    let searching = modal.is_searching(project);
    let Some(outcome) = modal.outcome() else {
        ui.label(muted(SEARCHING_LABEL));
        return;
    };
    match &outcome.result {
        Err(error) if !modal.is_stale(project) => {
            crate::theme::widgets::reason_label(ui, palette().ERROR_FG, error);
        }
        Err(_) => {
            ui.label(muted(SEARCHING_LABEL));
        }
        Ok(found) => {
            if searching {
                ui.label(muted(SEARCHING_LABEL));
            } else if rows == 0 {
                ui.label(muted(NO_MATCHES_LABEL));
            } else if found.is_full() && found.next_cursor.is_some() {
                ui.label(muted(REFINE_LABEL));
            }
            if let Some(notice) = index_notice(&found.index) {
                ui.colored_label(palette().WARNING_FG, notice);
            }
            if found.title_scan_limited {
                ui.colored_label(palette().WARNING_FG, TITLE_SCAN_LIMITED_LABEL);
            }
        }
    }
}

/// Why conversation bodies may be missing from the results, if they may be.
pub fn index_notice(index: &runtime::HostSearchIndex) -> Option<String> {
    if index.enabled == Some(false) {
        return Some(
            "Conversation bodies are not searched: the thread index is disabled in storage settings."
                .into(),
        );
    }
    if index.unindexed == 0 {
        return None;
    }
    let runs = index.unindexed;
    Some(if index.backfill_in_progress {
        format!("Indexing in progress: {runs} run(s) are not searchable by body yet.")
    } else {
        format!(
            "{runs} run(s) are not indexed; they match only by title, prompts and final report."
        )
    })
}

fn result_list(
    ui: &mut egui::Ui,
    modal: &ThreadSearchModal,
    rows: &[ResultRow],
    scroll_to_selected: bool,
) -> Option<ThreadId> {
    let selected = modal.selected(rows.len());
    let mut opened = None;
    egui::ScrollArea::vertical()
        .max_height(RESULT_MAX_HEIGHT)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for (index, row) in rows.iter().enumerate() {
                let response = result_row(ui, row, index == selected);
                if scroll_to_selected && index == selected {
                    response.scroll_to_me(None);
                }
                if response.clicked() {
                    opened = Some(row.thread.clone());
                }
            }
        });
    opened
}

fn result_row(ui: &mut egui::Ui, row: &ResultRow, selected: bool) -> egui::Response {
    let label = if row.archived {
        format!("{} (archived)", row.title)
    } else {
        row.title.clone()
    };
    let response = ui
        .scope(|ui| {
            ui.set_width(ui.available_width());
            let response = ui.selectable_label(selected, RichText::new(&label).strong());
            if let Some(snippet) = &row.snippet {
                let kind = row.kind.as_deref().unwrap_or("match");
                ui.horizontal(|ui| {
                    ui.add_space(SP_2);
                    ui.add(
                        egui::Label::new(muted(format!(
                            "{} {kind}: {snippet}",
                            icons::MAGNIFYING_GLASS
                        )))
                        .truncate(),
                    );
                });
            }
            response
        })
        .inner;
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
    response
}
