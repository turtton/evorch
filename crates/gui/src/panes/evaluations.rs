//! The Evaluations panel: evaluation groups, then a group's members (one per
//! thread), then a member's trials, its report and the replay of a finished
//! trial. It draws what [`EvaluationsModel`] holds and changes only the
//! model's selection; reading is the model's job on its workers.

use egui::{Color32, RichText, Ui};
use eval::inspection::{
    Artifact, ArtifactProblem, EvaluationGroupSnapshot, EvaluationMemberSnapshot,
    EvaluationTrialSnapshot, GroupPhase, MemberPhase, Purpose, TrialState,
};

use crate::model::evaluations::present;
use crate::model::evaluations::{EvaluationsModel, Loaded, ReplayData, ReplayView};
use crate::theme::text::{h3, h4, muted};
use crate::theme::tokens::{SP_1, SP_2, palette};
use crate::theme::widgets::{badge, empty_state};

/// Draws the panel. `visible` is whether the tab is on screen, which the
/// caller also gave to the model's poll.
pub fn evaluations_pane(ui: &mut Ui, model: &mut EvaluationsModel) {
    if !model.is_available() {
        empty_state(
            ui,
            "Evaluations are unavailable",
            "There is no user configuration directory to read evaluations from.",
            None,
        );
        return;
    }
    if model.note_rendered() {
        ui.ctx().request_repaint();
    }
    toolbar(ui, model);
    ui.separator();
    if model.replay().is_some() && model.selection().trial.is_some() {
        replay_page(ui, model);
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("evaluations-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if model.selection().group.is_none() {
                group_list(ui, model);
            } else if model.selection().member.is_none() {
                group_page(ui, model);
            } else {
                member_page(ui, model);
            }
        });
}

fn toolbar(ui: &mut Ui, model: &mut EvaluationsModel) {
    ui.horizontal_wrapped(|ui| {
        if ui.button("Refresh").clicked() {
            model.request_refresh();
            ui.ctx().request_repaint();
        }
        let selection = model.selection().clone();
        if selection.group.is_some() {
            ui.label(muted("›"));
            if ui.link("Groups").clicked() {
                model.clear_group();
            }
        }
        if let Some(group) = &selection.group {
            ui.label(muted("›"));
            if selection.member.is_some() {
                if ui.link(group).clicked() {
                    model.clear_member();
                }
            } else {
                ui.label(group);
            }
        }
        if let Some(member) = selection.member {
            ui.label(muted("›"));
            let thread = member_of(model, member).map(|m| m.thread_id.clone());
            let label = thread.unwrap_or_else(|| format!("member {}", member + 1));
            if selection.trial.is_some() {
                if ui.link(&label).clicked() {
                    model.close_replay();
                }
            } else {
                ui.label(label);
            }
        }
        if let Some(trial) = &selection.trial {
            ui.label(muted("›"));
            ui.label(format!("{trial} replay"));
        }
        if model.in_flight() > 0 {
            crate::theme::widgets::spinner(ui);
        }
        if let Some(failure) = model.failure() {
            ui.colored_label(palette().ERROR_FG, failure);
        }
    });
}

fn member_of(model: &EvaluationsModel, index: usize) -> Option<&EvaluationMemberSnapshot> {
    model.group()?.present()?.members.get(index)
}

fn problem_label(ui: &mut Ui, problem: &ArtifactProblem) {
    ui.colored_label(palette().WARNING_FG, format!("Artifact problem: {problem}"));
}

fn phase_color(running: bool, bad: bool, unverified: bool) -> Color32 {
    if running {
        palette().RUNNING
    } else if bad {
        palette().ERROR_FG
    } else if unverified {
        palette().WARNING_FG
    } else {
        palette().SUCCESS
    }
}

fn group_badge(ui: &mut Ui, phase: GroupPhase) {
    let color = phase_color(
        phase == GroupPhase::Running,
        phase == GroupPhase::Incomplete,
        matches!(phase, GroupPhase::Unverified | GroupPhase::Partial),
    );
    badge(
        ui,
        present::group_phase_label(phase),
        palette().CANVAS,
        color,
    );
}

fn member_badge(ui: &mut Ui, phase: MemberPhase) {
    let color = phase_color(
        phase.is_active(),
        matches!(
            phase,
            MemberPhase::Failed | MemberPhase::FinishedWithoutReport
        ),
        phase == MemberPhase::Unverified,
    );
    let response = badge(
        ui,
        present::member_phase_label(phase),
        palette().CANVAS,
        color,
    );
    if phase == MemberPhase::Unverified {
        response.on_hover_text(present::UNVERIFIED_HINT);
    }
}

fn group_list(ui: &mut Ui, model: &mut EvaluationsModel) {
    let Some(listing) = model.listing().cloned() else {
        ui.label(muted("Loading evaluations…"));
        return;
    };
    if let Some(problem) = &listing.problem {
        problem_label(ui, problem);
    }
    if listing.entries.is_empty() && listing.problem.is_none() {
        empty_state(
            ui,
            "No evaluations yet",
            "Evaluations the Evaluator starts appear here with their progress and reports.",
            None,
        );
        return;
    }
    for entry in &listing.entries {
        ui.horizontal_wrapped(|ui| {
            if ui
                .add(egui::Button::new(RichText::new(&entry.id).strong()).frame(false))
                .clicked()
            {
                model.select_group(&entry.id);
            }
            match &entry.group {
                Artifact::Present(group) => {
                    group_badge(ui, group.phase);
                    ui.label(muted(format!(
                        "{} · {}",
                        group.started_at,
                        members_summary(group)
                    )));
                }
                Artifact::Missing => {
                    ui.label(muted("record missing"));
                }
                Artifact::Problem(problem) => problem_label(ui, problem),
            }
        });
    }
    if listing.omitted > 0 {
        ui.label(muted(format!(
            "{} older groups not listed",
            listing.omitted
        )));
    }
    if listing.scan_truncated {
        ui.label(muted(
            "The groups directory is larger than what was scanned.",
        ));
    }
}

fn members_summary(group: &EvaluationGroupSnapshot) -> String {
    let finished: usize = group.members.iter().map(|m| m.finished_trials()).sum();
    let planned: usize = group.members.iter().map(|m| m.planned_trials()).sum();
    format!(
        "{} threads · {finished} of {planned} trials finished",
        group.members.len()
    )
}

fn group_page(ui: &mut Ui, model: &mut EvaluationsModel) {
    let Some(group) = model.group().cloned() else {
        ui.label(muted("Loading the group…"));
        return;
    };
    let group = match group {
        Artifact::Present(group) => group,
        Artifact::Missing => {
            ui.label("This evaluation group is not there (any more).");
            return;
        }
        Artifact::Problem(problem) => {
            problem_label(ui, &problem);
            return;
        }
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(h3(&group.id));
        group_badge(ui, group.phase);
    });
    ui.label(muted(format!(
        "Started {}{}",
        group.started_at,
        group
            .finished_at
            .as_ref()
            .map_or_else(String::new, |at| format!(" · finished {at}"))
    )));
    if group.phase == GroupPhase::Unverified {
        ui.colored_label(palette().WARNING_FG, present::UNVERIFIED_HINT);
    }
    if let Some(notice) = &group.notice {
        ui.label(muted(notice));
    }
    match &group.candidate {
        Artifact::Present(candidate) => {
            ui.collapsing("Candidate configuration", |ui| {
                ui.monospace(&candidate.text);
                if candidate.truncated {
                    ui.label(muted(format!(
                        "Showing the start of {} bytes.",
                        candidate.total_bytes
                    )));
                }
            });
        }
        Artifact::Missing => {}
        Artifact::Problem(problem) => problem_label(ui, problem),
    }
    ui.add_space(SP_2);
    ui.label(h4("Threads"));
    for member in &group.members {
        member_row(ui, model, member);
    }
    if group.members_omitted > 0 {
        ui.label(muted(format!(
            "{} more threads not shown",
            group.members_omitted
        )));
    }
}

fn purpose_label(purpose: Purpose) -> &'static str {
    purpose.as_str()
}

fn member_row(ui: &mut Ui, model: &mut EvaluationsModel, member: &EvaluationMemberSnapshot) {
    ui.horizontal_wrapped(|ui| {
        let label = format!("{} · {}", purpose_label(member.purpose), member.thread_id);
        if ui
            .add(egui::Button::new(RichText::new(label).strong()).frame(false))
            .clicked()
        {
            model.select_member(member.index);
        }
        member_badge(ui, member.phase);
        ui.label(muted(present::progress_text(member)));
        let (_, lines) = present::member_cost_lines(&member.cost);
        ui.label(muted(lines.join("; ")));
    });
}

fn member_page(ui: &mut Ui, model: &mut EvaluationsModel) {
    let Some(Artifact::Present(group)) = model.group().cloned() else {
        ui.label(muted("Loading the group…"));
        return;
    };
    let Some(member) = model
        .selection()
        .member
        .and_then(|index| group.members.get(index))
        .cloned()
    else {
        ui.label("This thread is not part of the group.");
        return;
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(h3(&member.thread_id));
        badge(
            ui,
            purpose_label(member.purpose),
            palette().CANVAS,
            palette().ACCENT,
        );
        member_badge(ui, member.phase);
    });
    if member.phase == MemberPhase::Unverified {
        ui.colored_label(palette().WARNING_FG, present::UNVERIFIED_HINT);
    }
    ui.label(format!(
        "Process: {}",
        present::process_text(&member.process)
    ));
    ui.label(present::progress_text(&member));
    if let Some(failure) = &member.failure {
        ui.colored_label(palette().ERROR_FG, format!("Failed: {failure}"));
        if let Some(tail) = member
            .stderr_tail
            .as_deref()
            .filter(|tail| !tail.is_empty())
        {
            ui.collapsing("Error output", |ui| {
                ui.monospace(tail);
            });
        }
    }
    if !member.problems.is_empty() {
        ui.add_space(SP_1);
        for problem in &member.problems {
            problem_label(ui, problem);
        }
    }
    ui.add_space(SP_2);
    let (source, lines) = present::member_cost_lines(&member.cost);
    ui.label(h4(source));
    for line in lines {
        ui.label(line);
    }
    ui.add_space(SP_2);
    ui.label(h4("Trials"));
    for trial in &member.trials {
        trial_row(ui, model, trial);
    }
    if member.trials_omitted > 0 {
        ui.label(muted(format!(
            "{} more trials not shown",
            member.trials_omitted
        )));
    }
    ui.add_space(SP_2);
    report_section(ui, model, &member);
}

fn trial_row(ui: &mut Ui, model: &mut EvaluationsModel, trial: &EvaluationTrialSnapshot) {
    let name = trial.name();
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(&name).strong());
        let running = matches!(trial.state, TrialState::Running | TrialState::Queued);
        let bad = matches!(trial.state, TrialState::Failed | TrialState::Interrupted);
        let color = phase_color(running, bad, matches!(trial.state, TrialState::Unverified));
        let response = badge(
            ui,
            present::trial_state_label(trial.state),
            palette().CANVAS,
            color,
        );
        if trial.state == TrialState::Unverified {
            response.on_hover_text(present::UNVERIFIED_HINT);
        }
        if matches!(trial.state, TrialState::Finished | TrialState::Failed) {
            ui.label(present::outcome_text(&trial.outcome));
        }
        let cost = ui.label(format!("cost {}", present::trial_cost_text(&trial.cost)));
        if let Some(reason) = present::trial_cost_reason(&trial.cost) {
            cost.on_hover_text(reason);
        }
        if matches!(trial.state, TrialState::Finished | TrialState::Failed)
            && ui.button(format!("Replay {name}")).clicked()
        {
            model.select_trial(&name);
        }
    });
}

fn report_section(ui: &mut Ui, model: &EvaluationsModel, member: &EvaluationMemberSnapshot) {
    ui.label(h4("Report"));
    use eval::inspection::ReportStatus;
    match &member.report {
        ReportStatus::Pending => {
            ui.label(muted(
                "The report is not there yet; the evaluation still runs.",
            ));
        }
        ReportStatus::Missing => {
            ui.label(muted("No report was produced."));
        }
        ReportStatus::Unreadable(problem) => problem_label(ui, problem),
        ReportStatus::Available(summary) => {
            ui.label(muted(format!(
                "Generated {} · {} pairs judged · {} warnings",
                summary.generated_at, summary.pairs_judged, summary.warnings
            )));
            for tally in &summary.tally {
                ui.label(format!(
                    "{}: candidate {} / baseline {} / ties {}",
                    tally.aspect, tally.candidate_wins, tally.baseline_wins, tally.ties
                ));
            }
        }
    }
    match model.markdown() {
        None => {
            if member.report_markdown.is_some() {
                ui.label(muted("report.md is not available."));
            }
        }
        Some(Loaded::Loading) => {
            ui.label(muted("Loading report.md…"));
        }
        Some(Loaded::Missing) => {
            ui.label(muted("report.md is not there."));
        }
        Some(Loaded::Problem(problem)) => {
            ui.colored_label(palette().WARNING_FG, format!("Artifact problem: {problem}"));
        }
        Some(Loaded::Ready(report)) => {
            if report.truncated {
                ui.label(muted(format!(
                    "Showing the start of report.md ({} of {} bytes).",
                    report.text.len(),
                    report.total_bytes
                )));
            }
            let id = format!(
                "evaluation-report-{}-{}",
                model.selection().group.as_deref().unwrap_or_default(),
                member.index
            );
            super::markdown_render::render_markdown(ui, &report.text, &id);
        }
    }
}

fn replay_page(ui: &mut Ui, model: &mut EvaluationsModel) {
    let Some((key, view)) = model
        .replay()
        .map(|(key, view)| (key.clone(), view.clone()))
    else {
        return;
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(h3(format!("{} replay", key.trial)));
        badge(ui, "read-only", palette().CANVAS, palette().TEXT_MUTED);
    });
    match view {
        ReplayView::Loading => {
            ui.label(muted("Reading the trial's conversation…"));
        }
        ReplayView::Ready(data) => {
            let selected = model
                .replay_run()
                .or_else(|| data.default_run())
                .map(str::to_owned);
            run_picker(ui, model, &data, selected.as_deref());
            ui.label(muted(format!(
                "{} events{}",
                data.events(),
                if data.truncated() {
                    " (the trial has more; this is the beginning)"
                } else {
                    ""
                }
            )));
            ui.separator();
            let Some(run) = selected else {
                ui.label("The trial recorded no runs.");
                return;
            };
            match data.transcript(&run) {
                Some(transcript) => {
                    let rows = agent_rows(&data);
                    ui.push_id(
                        (&key.group, key.member, &key.member_dir, &key.trial, &run),
                        |ui| {
                            super::agent::transcript_body_with_agents(ui, transcript, None, &rows);
                        },
                    );
                }
                None => {
                    ui.label("This run recorded no conversation.");
                }
            }
        }
        ReplayView::Fallback {
            reason,
            conversation,
        } => {
            ui.colored_label(
                palette().WARNING_FG,
                format!("The full replay is unavailable: {reason}"),
            );
            ui.label(muted(
                "What the user saw, from conversation.json (no tool calls):",
            ));
            ui.separator();
            egui::ScrollArea::vertical()
                .id_salt("evaluation-conversation-fallback")
                .auto_shrink([false, false])
                .show(ui, |ui| match conversation {
                    Artifact::Present(entries) => {
                        for entry in &entries {
                            conversation_entry(ui, entry);
                        }
                    }
                    Artifact::Missing => {
                        ui.label(muted("conversation.json is not there either."));
                    }
                    Artifact::Problem(problem) => problem_label(ui, &problem),
                });
        }
    }
}

fn conversation_entry(ui: &mut Ui, entry: &eval::conversation::Entry) {
    use eval::conversation::Entry;
    match entry {
        Entry::User { text } => labelled(ui, "User", text),
        Entry::Assistant { text } => labelled(ui, "Assistant", text),
        Entry::Question { title, options, .. } => {
            labelled(ui, "Question", title);
            for option in options {
                ui.label(muted(format!("  • {option}")));
            }
        }
        Entry::Answer { title, text, .. } => labelled(ui, &format!("Answer to “{title}”"), text),
    }
}

fn labelled(ui: &mut Ui, role: &str, text: &str) {
    ui.label(RichText::new(role).strong());
    ui.label(text);
    ui.add_space(SP_1);
}

fn run_picker(
    ui: &mut Ui,
    model: &mut EvaluationsModel,
    data: &ReplayData,
    selected: Option<&str>,
) {
    if data.runs().len() < 2 {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(muted("Run:"));
        for run in data.runs() {
            let indent = "  ".repeat(data.depth(run));
            let role = if run.role.is_empty() {
                String::new()
            } else {
                format!(" ({})", run.role)
            };
            let label = format!("{indent}{}{role}", run.name);
            if ui
                .selectable_label(selected == Some(run.run_id.as_str()), label)
                .clicked()
            {
                model.select_replay_run(&run.run_id);
            }
        }
    });
}

/// Names for the runs the transcript mentions, so delegation cards read by
/// agent name.
fn agent_rows(data: &ReplayData) -> Vec<crate::model::tasks::TaskRow> {
    data.runs()
        .iter()
        .filter_map(|run| {
            Some(crate::model::tasks::TaskRow {
                run_id: run.run_id.parse().ok()?,
                name: run.name.clone(),
                role: run.role.clone(),
                status: event_bus::AgentRunPhase::Done,
                model: String::new(),
                category: None,
                cancelled: false,
            })
        })
        .collect()
}
