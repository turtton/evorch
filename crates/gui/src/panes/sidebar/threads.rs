use std::collections::{BTreeMap, BTreeSet};

use egui::{Align, Layout, Sense, Ui};
use workspace_ui::{ProjectRecord, SidebarState, ThreadRecord, ThreadRunPhase, ThreadState};

use crate::model::telemetry::TelemetryOverlay;
use crate::theme::text::h4;
use crate::theme::tokens::{ROW_DENSE, SP_1, SP_2};
use crate::theme::tokens::{palette, state_color};
use crate::theme::widgets::{compact_row, empty_state, primary_button, status_dot};

use super::SidebarAction;

pub fn render(
    ui: &mut Ui,
    sidebar: &SidebarState,
    project: &ProjectRecord,
    phases: &BTreeMap<String, ThreadRunPhase>,
    _telemetry: &TelemetryOverlay,
    question_threads: &BTreeSet<workspace_ui::ThreadId>,
    action: &mut Option<SidebarAction>,
) {
    let (project_threads, archived) =
        ThreadRecord::partition_for_project(&sidebar.threads, &project.id);

    ui.separator();
    ui.horizontal(|ui| {
        ui.label(h4("Threads"));
        let has_threads = !project_threads.is_empty();
        let new_thread_clicked = if has_threads {
            ui.button("New thread").clicked()
        } else {
            primary_button(ui, "New thread").clicked()
        };
        if new_thread_clicked {
            let title = format!("thread-{}", sidebar.threads.len() + 1);
            *action = Some(SidebarAction::CreateThread(title));
        }
    });

    if project_threads.is_empty() {
        empty_state(
            ui,
            "No threads yet",
            "Start a thread to begin a conversation.",
            None,
        );
    }

    render_tree(
        ui,
        sidebar,
        &project_threads,
        phases,
        question_threads,
        false,
        action,
    );

    egui::CollapsingHeader::new(format!("アーカイブ済み ({})", archived.len()))
        .id_salt(("archived-threads", &project.id))
        .show(ui, |ui| {
            render_tree(
                ui,
                sidebar,
                &archived,
                phases,
                question_threads,
                true,
                action,
            );
        });
    ui.add_space(SP_2);
}

fn render_tree(
    ui: &mut Ui,
    sidebar: &SidebarState,
    threads: &[&ThreadRecord],
    phases: &BTreeMap<String, ThreadRunPhase>,
    question_threads: &BTreeSet<workspace_ui::ThreadId>,
    archived: bool,
    action: &mut Option<SidebarAction>,
) {
    let rows = nested_threads(threads);
    let mut collapsed_depth = None;
    for (index, (thread, depth)) in rows.iter().copied().enumerate() {
        if collapsed_depth.is_some_and(|hidden_depth| depth > hidden_depth) {
            continue;
        }
        collapsed_depth = None;
        let has_children = rows.get(index + 1).is_some_and(|(_, next)| *next > depth);
        // This ID deliberately excludes the active/archive section and parent
        // widget, so moving a family preserves each branch's expansion state.
        let id = egui::Id::new(("sidebar-thread-children", &thread.project_id, &thread.id));
        let mut expansion =
            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true);
        let active = sidebar.active_thread.as_ref() == Some(&thread.id);
        ui.push_id(&thread.id, |ui| {
            compact_row(ui, active, |ui| {
                ui.spacing_mut().item_spacing.x = SP_1;
                ui.spacing_mut().button_padding.x = SP_1;
                ui.add_space(depth as f32 * 16.0);
                if has_children {
                    let (rect, toggle) =
                        ui.allocate_exact_size(egui::vec2(16.0, ROW_DENSE), Sense::click());
                    if toggle.clicked() {
                        expansion.toggle(ui);
                    }
                    let label = if expansion.is_open() {
                        "Collapse"
                    } else {
                        "Expand"
                    };
                    toggle.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            true,
                            format!("{label} children of {}", thread.id),
                        )
                    });
                    egui::collapsing_header::paint_default_icon(
                        ui,
                        if expansion.is_open() { 1.0 } else { 0.0 },
                        &toggle.with_new_rect(rect.shrink(2.0)),
                    );
                }
                if archived {
                    archived_row(ui, thread, action);
                } else {
                    let state = thread.state(phases);
                    active_row(
                        ui,
                        thread,
                        state,
                        question_threads.contains(&thread.id),
                        action,
                    );
                }
            });
        });
        expansion.store(ui.ctx());
        if has_children && !expansion.is_open() {
            collapsed_depth = Some(depth);
        }
        if !archived && let (Some(branch), Some(worktree)) = (&thread.branch, &thread.worktree_path)
        {
            ui.label(crate::theme::text::muted(format!(
                "{branch} @ {}",
                worktree.display()
            )));
        }
    }
}

fn active_row(
    ui: &mut Ui,
    thread: &ThreadRecord,
    state: ThreadState,
    has_question: bool,
    action: &mut Option<SidebarAction>,
) {
    let pin = if thread.pinned { "★" } else { "☆" };
    if ui.button(pin).clicked() {
        *action = Some(SidebarAction::TogglePin(thread.id.clone()));
    }
    let label = format!("Thread status: {}", thread_state_label(state));
    let dot = status_dot(ui, state_color(state)).on_hover_text(&label);
    dot.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label));
    // A pending question does not change the run phase. Keep both signals.
    if has_question {
        ui.label(
            egui::RichText::new("?")
                .strong()
                .color(palette().WARNING_FG),
        )
        .on_hover_text("Answer needed: this thread has an unanswered question");
    }
    if thread.parent_thread_id.is_none() {
        let archive = ui
            .add_enabled(!thread.pinned, archive_button)
            .on_hover_text("アーカイブ");
        archive.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, !thread.pinned, "Archive")
        });
        if archive.clicked() {
            *action = Some(SidebarAction::ToggleArchive(thread.id.clone()));
        }
    } else {
        ui.add_space(ui.text_style_height(&egui::TextStyle::Small) + SP_2);
    }
    // At the minimum window size the two action buttons leave little
    // room for the title, and egui's minimum Label width overlaps earlier
    // controls. Keep the title and controls in distinct hit regions.
    let narrow = ui.available_width() < 180.0;
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if narrow {
            ui.menu_button("⋯", |ui| {
                let pause = if thread.paused { "Resume" } else { "Pause" };
                if ui.button(pause).clicked() {
                    *action = Some(SidebarAction::TogglePause(thread.id.clone()));
                    ui.close();
                }
                if ui.button("Fork").clicked() {
                    *action = Some(SidebarAction::ForkThread(thread.id.clone()));
                    ui.close();
                }
            })
            .response
            .on_hover_text("Thread actions");
        } else {
            let pause = if thread.paused { "Resume" } else { "Pause" };
            if ui.button(pause).clicked() {
                *action = Some(SidebarAction::TogglePause(thread.id.clone()));
            }
            if ui.small_button("Fork").clicked() {
                *action = Some(SidebarAction::ForkThread(thread.id.clone()));
            }
        }
        thread_title(ui, thread, action);
    });
}

fn archived_row(ui: &mut Ui, thread: &ThreadRecord, action: &mut Option<SidebarAction>) {
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if thread.parent_thread_id.is_none()
            && ui
                .small_button("Restore")
                .on_hover_text("アーカイブを解除")
                .clicked()
        {
            *action = Some(SidebarAction::ToggleArchive(thread.id.clone()));
        }
        thread_title(ui, thread, action);
    });
}

fn thread_title(ui: &mut Ui, thread: &ThreadRecord, action: &mut Option<SidebarAction>) {
    if ui
        .add_sized(
            egui::vec2(ui.available_width().max(0.0), ROW_DENSE),
            egui::Label::new(format!(
                "{}{}",
                if thread.parent_thread_id.is_some() {
                    "↳ "
                } else {
                    ""
                },
                thread.title
            ))
            .truncate()
            .halign(Align::LEFT)
            .sense(Sense::click()),
        )
        .on_hover_text(format!("thread ID: {}", thread.id))
        .clicked()
    {
        *action = Some(SidebarAction::SwitchThread(thread.id.clone()));
    }
}

fn nested_threads<'a>(threads: &[&'a ThreadRecord]) -> Vec<(&'a ThreadRecord, usize)> {
    fn append<'a>(
        thread: &'a ThreadRecord,
        depth: usize,
        threads: &[&'a ThreadRecord],
        seen: &mut BTreeSet<workspace_ui::ThreadId>,
        output: &mut Vec<(&'a ThreadRecord, usize)>,
    ) {
        if !seen.insert(thread.id.clone()) {
            return;
        }
        output.push((thread, depth));
        for child in threads
            .iter()
            .copied()
            .filter(|candidate| candidate.parent_thread_id.as_ref() == Some(&thread.id))
        {
            append(child, depth + 1, threads, seen, output);
        }
    }

    let mut seen = BTreeSet::new();
    let mut output = Vec::with_capacity(threads.len());
    for thread in threads.iter().copied() {
        if thread
            .parent_thread_id
            .as_ref()
            .is_none_or(|parent| !threads.iter().any(|candidate| &candidate.id == parent))
        {
            append(thread, 0, threads, &mut seen, &mut output);
        }
    }
    // Corrupt or cyclic parent links still leave every thread reachable.
    for thread in threads.iter().copied() {
        append(thread, 0, threads, &mut seen, &mut output);
    }
    output
}

fn archive_button(ui: &mut Ui) -> egui::Response {
    let size = ui.text_style_height(&egui::TextStyle::Small);
    let response = ui.add(
        egui::Button::new("")
            .small()
            .min_size(egui::vec2(size + SP_2, size + SP_2)),
    );
    let rect = egui::Rect::from_center_size(response.rect.center(), egui::vec2(size, size));
    let color = ui.style().interact(&response).text_color();
    paint_archive_box_icon(ui.painter(), rect, color);
    response
}

fn paint_archive_box_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let stroke = egui::Stroke::new(1.2, color);
    let point = |x: f32, y: f32| rect.min + egui::vec2(x * rect.width(), y * rect.height());
    painter.rect_stroke(
        egui::Rect::from_min_max(point(0.12, 0.36), point(0.88, 0.94)),
        0,
        stroke,
        egui::StrokeKind::Inside,
    );
    painter.rect_stroke(
        egui::Rect::from_min_max(point(0.0, 0.06), point(1.0, 0.36)),
        0,
        stroke,
        egui::StrokeKind::Inside,
    );
    painter.line_segment([point(0.38, 0.21), point(0.62, 0.21)], stroke);
}

const fn thread_state_label(state: ThreadState) -> &'static str {
    match state {
        ThreadState::Active => "Active",
        ThreadState::Paused => "Paused",
        ThreadState::Stopped => "Stopped (resumable)",
        ThreadState::Running => "Running",
        ThreadState::Waiting => "Waiting",
        ThreadState::Done => "Done",
        ThreadState::Error => "Error",
    }
}

#[cfg(test)]
mod tests {
    use super::nested_threads;
    use workspace_ui::{ProjectId, ThreadId, ThreadRecord};

    #[test]
    fn escalation_children_follow_parent_at_each_depth() {
        let project = ProjectId::new("demo");
        let parent = ThreadRecord::new(ThreadId::new("parent"), project.clone(), "Parent");
        let mut child = ThreadRecord::new(ThreadId::new("child"), project.clone(), "Child");
        child.parent_thread_id = Some(parent.id.clone());
        child.escalation_source_run_id = Some("run-worker".into());
        let mut grandchild = ThreadRecord::new(ThreadId::new("grandchild"), project, "Grandchild");
        grandchild.parent_thread_id = Some(child.id.clone());
        let rows = nested_threads(&[&grandchild, &child, &parent]);
        assert_eq!(
            rows.iter()
                .map(|(thread, depth)| (thread.id.to_string(), *depth))
                .collect::<Vec<_>>(),
            [
                ("parent".into(), 0),
                ("child".into(), 1),
                ("grandchild".into(), 2)
            ]
        );
    }
}
