//! The Projects tab: the toolbar, the projects and their thread trees, drawn with the
//! embedded workbench's sidebar pieces. Clicks become navigation, intents sent with
//! the revision the core reported, or changes of this window's view.
use egui::collapsing_header::CollapsingState;
use egui::{Align, Layout, Ui};
use workbench_protocol::ids::{ProjectId, ThreadId};
use workspace_ui::ProjectSort;

use super::tree::{self, ThreadRow};
use super::{Dialog, Intent, Requests, SidebarModel, WorkspaceFeature};
use crate::core_client::cache::Cache;
use crate::core_client::features::{FeatureContext, Navigation, Selection};
use crate::panes::sidebar_widgets::{
    FAMILY_GAP, FAMILY_PAGE, Hidden, LEGACY_SORT_CHOICES, ProjectRowView, ToolbarAction,
    ToolbarView, TrailingView, TreeRails, TreeRowLead, attention_note, family_gap_before,
    family_limit_id, hidden_count_badge, limit_families, project_row, restore_button, roll_up,
    sidebar_toolbar, title_label, trailing_controls, tree_row_lead,
};
use crate::theme::text::muted;
use crate::theme::tokens::{SP_1, SP_2, palette};
use crate::theme::widgets::{compact_row_with, empty_state, ghost, reason_label};

/// The orders the core client offers: the core does not say in which order projects
/// were registered, so that one is left out.
fn sort_choices() -> &'static [(ProjectSort, &'static str)] {
    &LEGACY_SORT_CHOICES[1..]
}

struct Scope<'a> {
    cache: &'a Cache,
    model: &'a SidebarModel,
    selection: &'a Selection,
    project: &'a ProjectId,
}

struct Rename<'a> {
    editing: &'a mut Option<(String, String)>,
    focus: &'a mut bool,
}

impl WorkspaceFeature {
    pub(super) fn draw_sidebar(
        &mut self,
        ui: &mut Ui,
        ctx: &FeatureContext<'_>,
        requests: &mut Requests,
    ) {
        let Self {
            model,
            view,
            renaming,
            focus_rename,
            unread,
            dialog,
            notice,
            confirmation,
            ..
        } = self;
        let cache = ctx.cache();
        let selection = ctx.selection();
        let listed = model.project_list(cache);
        let order = tree::ordered_projects(&listed, cache, model, view.sort);
        let toolbar = ToolbarView {
            has_projects: !order.is_empty(),
            any_open: order
                .iter()
                .any(|project| !view.collapsed.contains(project.id.as_str())),
            sort: view.sort,
            choices: sort_choices(),
        };
        match sidebar_toolbar(ui, toolbar) {
            Some(ToolbarAction::AddProject) => *dialog = Some(Dialog::AddProject),
            Some(ToolbarAction::Sort(sort)) => view.sort = sort,
            Some(ToolbarAction::SetAllCollapsed(true)) => {
                view.collapsed = order
                    .iter()
                    .map(|project| project.id.as_str().to_owned())
                    .collect();
            }
            Some(ToolbarAction::SetAllCollapsed(false)) => view.collapsed.clear(),
            None => {}
        }
        egui::ScrollArea::vertical()
            .id_salt("sidebar-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if order.is_empty() {
                    empty_state(
                        ui,
                        "No projects yet",
                        "Add a repository root to start orchestrating.",
                        None,
                    );
                }
                for project in &order {
                    let id = project.id.as_str();
                    let open = !view.collapsed.contains(id);
                    let clicks = ui
                        .push_id(id, |ui| {
                            project_row(
                                ui,
                                ProjectRowView {
                                    name: &project.display_name,
                                    hover_root: model.access.get(id).map(|a| a.root.as_str()),
                                    selected: selection.project.as_ref() == Some(&project.id),
                                    open,
                                    count: tree::live_count(cache, model, id),
                                    search: false,
                                    settings: true,
                                },
                            )
                        })
                        .inner;
                    if clicks.toggle {
                        if open {
                            view.collapsed.insert(id.to_owned());
                        } else {
                            view.collapsed.remove(id);
                        }
                    }
                    if clicks.select {
                        requests
                            .navigations
                            .push(Navigation::SelectProject(project.id.clone()));
                    }
                    if clicks.new_thread {
                        requests
                            .navigations
                            .push(Navigation::NewConversation(project.id.clone()));
                    }
                    if clicks.settings {
                        *dialog = Some(Dialog::Settings(id.to_owned()));
                    }
                    if open {
                        let scope = Scope {
                            cache,
                            model,
                            selection,
                            project: &project.id,
                        };
                        let mut rename = Rename {
                            editing: renaming,
                            focus: focus_rename,
                        };
                        ui.push_id(("threads", id), |ui| {
                            threads(ui, &scope, unread, &mut rename, requests);
                        });
                    } else {
                        let limit = family_limit_id(id);
                        ui.ctx().data_mut(|data| data.remove::<usize>(limit));
                    }
                }
                // While a modal is open it shows the outcome itself.
                if dialog.is_none() {
                    if let Some(notice) = notice.as_deref() {
                        reason_label(ui, palette().ERROR_FG, notice);
                    }
                    if let Some(confirmation) = confirmation.as_deref() {
                        ui.label(muted(confirmation));
                    }
                }
            });
    }
}

fn threads(
    ui: &mut Ui,
    scope: &Scope<'_>,
    unread: &std::collections::BTreeSet<String>,
    rename: &mut Rename<'_>,
    requests: &mut Requests,
) {
    let project = scope.project.as_str();
    let tree = tree::project_tree(scope.cache, scope.model, project, unread);
    if tree.main.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(SP_2 + 16.0);
            ui.label(muted("No threads yet"));
        });
    }
    let limit_id = family_limit_id(project);
    let limit = ui
        .ctx()
        .data(|data| data.get_temp::<usize>(limit_id))
        .unwrap_or(FAMILY_PAGE);
    let selected = scope.selection.thread.as_ref().map(ThreadId::as_str);
    let (rows, hidden) = limit_families(&tree.main, limit, |row| {
        row.pinned
            || selected == Some(row.id())
            || row.own.question
            || row.own.approval
            || row.own.unread
    });
    draw_tree(ui, scope, &rows, false, rename, requests);
    if hidden > 0 {
        ui.indent(("more-threads-indent", project), |ui| {
            if ui
                .add(ghost(muted(format!("See more ({hidden})"))))
                .on_hover_text(format!("Show up to {FAMILY_PAGE} more threads"))
                .clicked()
            {
                ui.ctx()
                    .data_mut(|data| data.insert_temp(limit_id, limit + FAMILY_PAGE));
            }
        });
    }
    if !tree.archived.is_empty() {
        ui.indent(("archived-threads-indent", project), |ui| {
            egui::CollapsingHeader::new(format!("Archived ({})", tree.archived.len()))
                .id_salt(("archived-threads", project))
                .show(ui, |ui| {
                    draw_tree(ui, scope, &tree.archived, true, rename, requests);
                });
        });
    }
}

fn draw_tree(
    ui: &mut Ui,
    scope: &Scope<'_>,
    rows: &[(ThreadRow<'_>, usize)],
    archived: bool,
    rename: &mut Rename<'_>,
    requests: &mut Requests,
) {
    let rails = TreeRails::new(rows);
    let clip = ui.clip_rect();
    // Rails bridge the vertical gap between rows so a branch reads as one line.
    let row_gap = ui.spacing().item_spacing.y;
    let project_root = scope
        .model
        .access
        .get(scope.project.as_str())
        .map(|access| access.root.as_str());
    let mut collapsed_depth = None;
    for (index, &(row, depth)) in rows.iter().enumerate() {
        if collapsed_depth.is_some_and(|hidden_depth| depth > hidden_depth) {
            continue;
        }
        collapsed_depth = None;
        let has_children = rows.get(index + 1).is_some_and(|(_, next)| *next > depth);
        if family_gap_before(rows, index) {
            ui.add_space(FAMILY_GAP);
        }
        // Excludes the section, so a family moving between sections keeps its folds.
        let id = egui::Id::new(("sidebar-thread-children", scope.project.as_str(), row.id()));
        let mut expansion = CollapsingState::load_with_default_open(ui.ctx(), id, true);
        let hidden = if has_children && !expansion.is_open() {
            roll_up(rows, index, |row| (row.running, row.own))
        } else {
            Hidden::default()
        };
        let colors = if archived {
            None
        } else {
            row.own.merge(hidden.attention).resolve().colors(&palette())
        };
        let active = scope.selection.thread.as_ref().map(ThreadId::as_str) == Some(row.id());
        ui.push_id(row.id(), |ui| {
            let response = compact_row_with(ui, active, colors.map(|(_, tone)| tone), |ui| {
                tree_row_lead(
                    ui,
                    &TreeRowLead {
                        rails: &rails,
                        index,
                        depth,
                        has_children,
                        kind: row.kind,
                        clip,
                        row_gap,
                    },
                    &mut expansion,
                    row.id(),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let note = if archived {
                        ui.spacing_mut().item_spacing.x = SP_1;
                        if row.display_root && restore_button(ui).clicked() {
                            requests
                                .asks
                                .push((row.id().to_owned(), Intent::Archive(false)));
                        }
                        hidden_count_badge(ui, hidden.count);
                        None
                    } else {
                        let clicks = trailing_controls(
                            ui,
                            TrailingView {
                                status: row.status,
                                own: row.own,
                                hidden,
                                display_root: row.display_root,
                                // Archive waits for the detail: until then the row may be
                                // a pinned or running family's root shown as a plain one.
                                family_running: row.family_running || !row.detail_known,
                                pinned: row.pinned,
                            },
                        );
                        if clicks.archive {
                            requests
                                .asks
                                .push((row.id().to_owned(), Intent::Archive(true)));
                        }
                        if clicks.pin {
                            requests
                                .asks
                                .push((row.id().to_owned(), Intent::Pin(!row.pinned)));
                        }
                        attention_note(row.own, hidden.attention)
                    };
                    title(
                        ui,
                        scope.project,
                        &row,
                        (note, colors.map(|(fg, _)| fg)),
                        rename,
                        requests,
                    );
                });
            });
            if let Some(note) = attention_note(row.own, hidden.attention).filter(|_| !archived) {
                let label = format!("Thread attention: {note}");
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label)
                });
            }
        });
        expansion.store(ui.ctx());
        if has_children && !expansion.is_open() {
            collapsed_depth = Some(depth);
        }
        if !archived
            && let (Some(root), Some(project_root)) = (row.active_root, project_root)
            && root != project_root
        {
            ui.label(muted(format!("Shared workspace: {root}")));
        }
    }
}

/// The row's title, or the field its new title is typed in.
fn title(
    ui: &mut Ui,
    project: &ProjectId,
    row: &ThreadRow<'_>,
    (note, color): (Option<&str>, Option<egui::Color32>),
    rename: &mut Rename<'_>,
    requests: &mut Requests,
) {
    let id = row.id();
    if let Some((editing, text)) = rename.editing.as_mut()
        && editing == id
    {
        let field = ui.add(egui::TextEdit::singleline(text).desired_width(ui.available_width()));
        if std::mem::take(rename.focus) {
            field.request_focus();
        }
        if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
            *rename.editing = None;
        } else if field.lost_focus() {
            let title = text.trim().to_owned();
            if !title.is_empty() && title != row.title {
                requests.asks.push((id.to_owned(), Intent::Rename(title)));
            }
            *rename.editing = None;
        }
        return;
    }
    let hover = match note {
        Some(note) => format!("{note}\nthread ID: {id}"),
        None => format!("thread ID: {id}"),
    };
    let response = title_label(ui, row.title, color, !row.display_root, false, &hover);
    response.context_menu(|ui| {
        if ui.button("Rename…").clicked() {
            *rename.editing = Some((id.to_owned(), row.title.to_owned()));
            *rename.focus = true;
            ui.close();
        }
        if ui.button("Copy thread ID").clicked() {
            ui.ctx().copy_text(id.to_owned());
            ui.close();
        }
        if ui.button("Copy thread title").clicked() {
            ui.ctx().copy_text(row.title.to_owned());
            ui.close();
        }
    });
    if response.clicked() {
        requests.navigations.push(Navigation::SelectThread {
            project: project.clone(),
            thread: row.summary.id.clone(),
        });
    }
}
