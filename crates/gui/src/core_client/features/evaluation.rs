//! Desktop feature of the `evaluation` area, read side (PR 13): the evaluation
//! groups on disk, one group opened for its members, and a member's report. PR
//! 17c adds the jobs (run, report, rejudge) to this file.
//!
//! A file that is missing, one that cannot be used and one that is there are shown
//! as three different things; the core guesses none of them. Nothing here shows a
//! path.
//!
//! The screen pages as the legacy Evaluations panel does: the groups, then one
//! group's threads, then one thread's report, with a breadcrumb back.
use std::collections::BTreeMap;

use super::diagnostics::read::{Phase, Read, phase_label};
use super::layout::{bounded_list, paged_list};
use super::{Availability, Feature, FeatureContext, FeatureId, FeatureReply};
use crate::panes::evaluation_widgets::{
    UNVERIFIED_HINT, crumb, group_badge, member_badge, member_is_active, problem_line, problem_word,
};
use crate::panes::markdown_render::render_markdown;
use crate::theme::text::{h3, h4, muted};
use crate::theme::tokens::{SP_2, palette};
use crate::theme::widgets::empty_state;
use workbench_protocol::{
    events::UiEvent,
    handshake::Capability,
    ids::{EvaluationGroupId, EvaluationMemberId, PageCursor},
    management::{ArtifactPresence, EvaluationArtifactRef},
    namespaces::evaluation::{
        EvaluationEvent, EvaluationGroupDetail, EvaluationGroupPhase, EvaluationGroupSummary,
        EvaluationMemberPhase, EvaluationMemberSummary, EvaluationProblemKind, EvaluationQuery,
        EvaluationReply, EvaluationReportText, GetEvaluation, GetEvaluationReport, ListEvaluations,
    },
    queries::{PageRequest, Query, QueryReply},
};

#[derive(Debug, Clone, Copy)]
enum Pending {
    List { generation: u64, append: bool },
    Group { generation: u64 },
    Report { generation: u64 },
}

pub struct EvaluationFeature {
    next_tag: u64,
    pending: BTreeMap<u64, Pending>,
    list_read: Read,
    list_request: Option<ListEvaluations>,
    groups: Vec<EvaluationGroupSummary>,
    omitted: u32,
    scan_truncated: bool,
    list_problem: Option<EvaluationProblemKind>,
    next_cursor: Option<PageCursor>,
    list_due: bool,
    more_requested: bool,
    selected: Option<EvaluationGroupId>,
    group_read: Read,
    group_request: Option<GetEvaluation>,
    group: Option<EvaluationGroupDetail>,
    group_due: bool,
    member: Option<EvaluationMemberId>,
    report_read: Read,
    report_request: Option<GetEvaluationReport>,
    report: Option<EvaluationReportText>,
    report_due: bool,
}

impl Default for EvaluationFeature {
    fn default() -> Self {
        Self {
            next_tag: 0,
            pending: BTreeMap::new(),
            list_read: Read::default(),
            list_request: None,
            groups: Vec::new(),
            omitted: 0,
            scan_truncated: false,
            list_problem: None,
            next_cursor: None,
            list_due: true,
            more_requested: false,
            selected: None,
            group_read: Read::default(),
            group_request: None,
            group: None,
            group_due: false,
            member: None,
            report_read: Read::default(),
            report_request: None,
            report: None,
            report_due: false,
        }
    }
}

/// How an artifact stands: three states, never merged.
fn presence_word(artifact: &EvaluationArtifactRef) -> &'static str {
    match artifact.presence {
        ArtifactPresence::Present => "available",
        ArtifactPresence::Missing => "missing",
        ArtifactPresence::Problem => "unusable",
    }
}

fn member_title(member: &EvaluationMemberSummary) -> String {
    if member.thread_id.is_empty() {
        format!("member {}", member.index + 1)
    } else {
        member.thread_id.clone()
    }
}

fn progress_text(member: &EvaluationMemberSummary) -> String {
    let trials = &member.trials;
    let mut text = format!("{} of {} trials finished", trials.finished, trials.planned);
    if trials.failed > 0 {
        text.push_str(&format!(", {} failed", trials.failed));
    }
    if trials.running > 0 {
        text.push_str(&format!(", {} running", trials.running));
    }
    text
}

/// What the breadcrumb or a row asked the screen to show.
enum Go {
    Groups,
    Group(EvaluationGroupId),
    Member(EvaluationMemberId),
    GroupOnly,
}

impl EvaluationFeature {
    fn tag(&mut self, pending: Pending) -> u64 {
        self.next_tag += 1;
        self.pending.insert(self.next_tag, pending);
        self.next_tag
    }

    fn begin_list(&mut self, ctx: &mut FeatureContext<'_>) {
        self.groups.clear();
        self.next_cursor = None;
        let request = ListEvaluations {
            operation_id: self.list_read.begin("evaluation-list"),
            page: PageRequest::default(),
        };
        self.ask_list(ctx, request, None);
    }

    fn ask_list(
        &mut self,
        ctx: &mut FeatureContext<'_>,
        mut request: ListEvaluations,
        cursor: Option<PageCursor>,
    ) {
        let append = cursor.is_some();
        request.page = PageRequest {
            cursor,
            ..PageRequest::default()
        };
        let tag = self.tag(Pending::List {
            generation: self.list_read.generation(),
            append,
        });
        ctx.query(
            tag,
            Query::Evaluation(EvaluationQuery::ListEvaluations(request.clone())),
        );
        request.page = PageRequest::default();
        self.list_request = Some(request);
    }

    /// Re-reads the selected group. The open member, if any, stays: a Refresh or a
    /// reconnect on a thread's report page re-reads the report too, after this.
    fn begin_group(&mut self, ctx: &mut FeatureContext<'_>, group: EvaluationGroupId) {
        self.group = None;
        let request = GetEvaluation {
            operation_id: self.group_read.begin("evaluation-group"),
            group,
        };
        self.ask_group(ctx, request);
    }

    fn ask_group(&mut self, ctx: &mut FeatureContext<'_>, request: GetEvaluation) {
        let tag = self.tag(Pending::Group {
            generation: self.group_read.generation(),
        });
        ctx.query(
            tag,
            Query::Evaluation(EvaluationQuery::GetEvaluation(request.clone())),
        );
        self.group_request = Some(request);
    }

    fn begin_report(
        &mut self,
        ctx: &mut FeatureContext<'_>,
        group: EvaluationGroupId,
        member: EvaluationMemberId,
    ) {
        self.report = None;
        let request = GetEvaluationReport {
            operation_id: self.report_read.begin("evaluation-report"),
            group,
            member,
        };
        self.ask_report(ctx, request);
    }

    fn ask_report(&mut self, ctx: &mut FeatureContext<'_>, request: GetEvaluationReport) {
        let tag = self.tag(Pending::Report {
            generation: self.report_read.generation(),
        });
        ctx.query(
            tag,
            Query::Evaluation(EvaluationQuery::GetEvaluationReport(request.clone())),
        );
        self.report_request = Some(request);
    }
}

impl Feature for EvaluationFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Evaluation
    }

    fn title(&self) -> &'static str {
        "Evaluations"
    }

    fn availability(&self) -> Availability {
        Availability::Implemented {
            required: &[Capability::Queries],
        }
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.list_read.forget();
        self.group_read.forget();
        self.report_read.forget();
        self.groups.clear();
        self.next_cursor = None;
        self.group = None;
        self.report = None;
        self.list_due = true;
        self.group_due = self.selected.is_some();
        self.report_due = self.member.is_some();
    }

    fn event(&mut self, event: &UiEvent) {
        let UiEvent::Evaluation(EvaluationEvent::EvaluationOperationChanged { view }) = event
        else {
            return;
        };
        self.list_read.event(view);
        self.group_read.event(view);
        self.report_read.event(view);
    }

    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        let Some(pending) = self.pending.remove(&tag) else {
            return;
        };
        let FeatureReply::Query(result) = reply else {
            return;
        };
        let reply = match result {
            Ok(QueryReply::Evaluation(reply)) => reply,
            Ok(_) => return,
            Err(error) => {
                match pending {
                    Pending::List { generation, .. } => {
                        self.list_read.fail(generation, error.message);
                    }
                    Pending::Group { generation } => {
                        self.group_read.fail(generation, error.message);
                    }
                    Pending::Report { generation } => {
                        self.report_read.fail(generation, error.message);
                    }
                }
                return;
            }
        };
        match (pending, reply) {
            (Pending::List { generation, append }, EvaluationReply::List(outcome)) => {
                if let Some(list) =
                    self.list_read
                        .accept(generation, &outcome.operation, outcome.data)
                {
                    self.omitted = list.omitted;
                    self.scan_truncated = list.scan_truncated;
                    self.list_problem = list.problem;
                    if append {
                        for group in list.groups.items {
                            if !self.groups.iter().any(|known| known.group == group.group) {
                                self.groups.push(group);
                            }
                        }
                    } else {
                        self.groups = list.groups.items;
                    }
                    self.next_cursor = list.groups.next_cursor;
                }
            }
            (Pending::Group { generation }, EvaluationReply::Group(outcome)) => {
                if let Some(group) =
                    self.group_read
                        .accept(generation, &outcome.operation, outcome.data)
                {
                    self.group = Some(group);
                }
            }
            (Pending::Report { generation }, EvaluationReply::Report(outcome)) => {
                if let Some(report) =
                    self.report_read
                        .accept(generation, &outcome.operation, outcome.data)
                {
                    self.report = Some(report);
                }
            }
            _ => {}
        }
    }

    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        let go = self.toolbar(ui);
        ui.separator();
        let go = go.or(if self.selected.is_none() {
            self.group_list(ui)
        } else if self.member.is_none() {
            self.group_page(ui)
        } else {
            self.member_page(ui);
            None
        });
        if let Some(go) = go {
            self.go(go);
        }

        if std::mem::take(&mut self.list_due) {
            self.begin_list(ctx);
        } else if self.list_read.take_ask_again()
            && let Some(request) = self.list_request.clone()
        {
            self.ask_list(ctx, request, None);
        }
        if std::mem::take(&mut self.more_requested)
            && let Some(request) = self.list_request.clone()
        {
            let cursor = self.next_cursor.clone();
            self.ask_list(ctx, request, cursor);
        }
        if std::mem::take(&mut self.group_due)
            && let Some(group) = self.selected.clone()
        {
            self.begin_group(ctx, group);
        } else if self.group_read.take_ask_again()
            && let Some(request) = self.group_request.clone()
        {
            self.ask_group(ctx, request);
        }
        if std::mem::take(&mut self.report_due)
            && let (Some(group), Some(member)) = (self.selected.clone(), self.member.clone())
        {
            self.begin_report(ctx, group, member);
        } else if self.report_read.take_ask_again()
            && let Some(request) = self.report_request.clone()
        {
            self.ask_report(ctx, request);
        }
    }
}

impl EvaluationFeature {
    fn go(&mut self, go: Go) {
        match go {
            Go::Groups => {
                self.selected = None;
                self.group = None;
                self.group_read.forget();
                self.group_request = None;
                self.forget_member();
            }
            Go::Group(group) => {
                self.forget_member();
                self.selected = Some(group);
                self.group_due = true;
            }
            Go::GroupOnly => self.forget_member(),
            Go::Member(member) => {
                self.member = Some(member);
                self.report_due = true;
            }
        }
    }

    fn forget_member(&mut self) {
        self.member = None;
        self.report = None;
        self.report_read.forget();
        self.report_request = None;
        self.report_due = false;
    }

    fn shown_member(&self) -> Option<&EvaluationMemberSummary> {
        let member = self.member.as_ref()?;
        self.group
            .as_ref()?
            .members
            .iter()
            .find(|shown| &shown.member == member)
    }

    /// Refresh, the breadcrumb, and what the page in front is waiting for.
    fn toolbar(&mut self, ui: &mut egui::Ui) -> Option<Go> {
        let mut go = None;
        ui.horizontal_wrapped(|ui| {
            if ui.button("Refresh").clicked() {
                self.list_due = true;
                self.group_due = self.selected.is_some();
                self.report_due = self.member.is_some();
            }
            if let Some(group) = &self.selected {
                crumb(ui);
                if ui.link("Groups").clicked() {
                    go = Some(Go::Groups);
                }
                crumb(ui);
                if self.member.is_some() {
                    if ui.link(group.as_str()).clicked() {
                        go = Some(Go::GroupOnly);
                    }
                } else {
                    ui.label(group.as_str());
                }
            }
            if let Some(member) = &self.member {
                crumb(ui);
                ui.label(
                    self.shown_member()
                        .map_or_else(|| member.as_str().to_owned(), member_title),
                );
            }
            let read = if self.member.is_some() && self.group.is_some() {
                &self.report_read
            } else if self.selected.is_some() {
                &self.group_read
            } else {
                &self.list_read
            };
            phase_label(ui, read.phase());
        });
        go
    }

    fn group_list(&mut self, ui: &mut egui::Ui) -> Option<Go> {
        if let Some(problem) = self.list_problem {
            problem_line(ui, problem_word(problem));
        }
        if self.list_read.phase() == &Phase::Ready
            && self.groups.is_empty()
            && self.list_problem.is_none()
        {
            empty_state(
                ui,
                "No evaluations yet",
                "Evaluations the Evaluator starts appear here with their progress and reports.",
                None,
            );
            return None;
        }
        let mut open = None;
        let groups = &self.groups;
        let more = paged_list(
            ui,
            "evaluation-groups",
            self.next_cursor.is_some().then_some("Load more groups"),
            |ui| {
                for group in groups {
                    ui.push_id(group.group.as_str(), |ui| {
                        ui.horizontal_wrapped(|ui| {
                            let id = egui::RichText::new(group.group.as_str()).strong();
                            if ui.add(egui::Button::new(id).frame(false)).clicked() {
                                open = Some(group.group.clone());
                            }
                            match group.presence {
                                ArtifactPresence::Present => {
                                    if let Some(phase) = group.phase {
                                        group_badge(ui, phase);
                                    }
                                    ui.label(muted(format!(
                                        "{} · {} threads",
                                        group.started_at.as_deref().unwrap_or("—"),
                                        group.members_total.unwrap_or(0)
                                    )));
                                }
                                ArtifactPresence::Missing => {
                                    ui.label(muted("record missing"));
                                }
                                ArtifactPresence::Problem => {
                                    problem_line(
                                        ui,
                                        group.problem.map_or("unusable", problem_word),
                                    );
                                }
                            }
                        });
                    });
                }
            },
        );
        if more {
            self.more_requested = true;
        }
        if self.omitted > 0 {
            ui.label(muted(format!("{} older groups not listed", self.omitted)));
        }
        if self.scan_truncated {
            ui.label(muted(
                "The groups directory is larger than what was scanned.",
            ));
        }
        open.map(Go::Group)
    }

    fn group_page(&mut self, ui: &mut egui::Ui) -> Option<Go> {
        let group = self.group.as_ref()?;
        ui.horizontal_wrapped(|ui| {
            ui.label(h3(group.group.as_str()));
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
        if group.phase == EvaluationGroupPhase::Unverified {
            ui.colored_label(palette().WARNING_FG, UNVERIFIED_HINT);
        }
        if let Some(notice) = &group.notice {
            ui.label(muted(notice));
        }
        if let Some(candidate) = &group.candidate {
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
        ui.add_space(SP_2);
        ui.label(h4("Threads"));
        let mut open = None;
        bounded_list(ui, "evaluation-members", |ui| {
            for member in &group.members {
                ui.push_id(member.member.as_str(), |ui| {
                    ui.horizontal_wrapped(|ui| {
                        let label = if member.thread_id.is_empty() {
                            member.member.as_str().to_owned()
                        } else {
                            format!("{} · {}", member.member.as_str(), member.thread_id)
                        };
                        let label = egui::RichText::new(label).strong();
                        if ui.add(egui::Button::new(label).frame(false)).clicked() {
                            open = Some(member.member.clone());
                        }
                        member_badge(ui, member.phase);
                        ui.label(muted(progress_text(member)));
                        ui.label(muted(format!(
                            "Report: {} · Replay: {}",
                            presence_word(&member.report),
                            presence_word(&member.replay)
                        )));
                        if let Some(failure) = &member.failure {
                            ui.colored_label(palette().ERROR_FG, format!("Failed: {failure}"));
                        }
                        if member.problems > 0 {
                            ui.colored_label(
                                palette().WARNING_FG,
                                format!("{} artifact problems", member.problems),
                            );
                        }
                    });
                });
            }
        });
        if group.members_omitted > 0 {
            ui.label(muted(format!(
                "{} more threads not shown",
                group.members_omitted
            )));
        }
        open.map(Go::Member)
    }

    /// The member's page. While its group is re-read (Refresh, a reconnect) the
    /// summary is gone for a moment: the page keeps its place under the member id.
    fn member_page(&self, ui: &mut egui::Ui) {
        let (Some(group), Some(member_id)) = (self.selected.as_ref(), self.member.as_ref()) else {
            return;
        };
        let shown = self.shown_member();
        ui.horizontal_wrapped(|ui| {
            ui.label(h3(
                shown.map_or_else(|| member_id.as_str().to_owned(), member_title)
            ));
            if let Some(member) = shown {
                member_badge(ui, member.phase);
            }
        });
        if let Some(member) = shown {
            if member.phase == EvaluationMemberPhase::Unverified {
                ui.colored_label(palette().WARNING_FG, UNVERIFIED_HINT);
            }
            ui.label(progress_text(member));
            if let Some(failure) = &member.failure {
                ui.colored_label(palette().ERROR_FG, format!("Failed: {failure}"));
            }
        }
        ui.add_space(SP_2);
        ui.label(h4("Report"));
        if let Some(report) = &self.report {
            match (report.artifact.presence, &report.text) {
                (ArtifactPresence::Present, Some(text)) => {
                    if text.truncated {
                        ui.label(muted(format!(
                            "Showing the start of report.md ({} of {} bytes).",
                            text.text.len(),
                            text.total_bytes
                        )));
                    }
                    let id = format!(
                        "core-evaluation-report-{}-{}",
                        group.as_str(),
                        member_id.as_str()
                    );
                    render_markdown(ui, &text.text, &id);
                }
                (ArtifactPresence::Problem, _) => {
                    problem_line(ui, report.problem.map_or("unusable", problem_word));
                }
                _ if shown.is_some_and(|member| member_is_active(member.phase)) => {
                    ui.label(muted(
                        "The report is not there yet; the evaluation still runs.",
                    ));
                }
                _ => {
                    ui.label(muted("No report was produced."));
                }
            }
        }
        match shown.map(|member| member.replay.presence) {
            Some(ArtifactPresence::Present) => {
                ui.label(muted(
                    "Replay: available (opening it needs the core client's replay view)",
                ));
            }
            Some(ArtifactPresence::Problem) => {
                ui.label(muted("Replay: unusable"));
            }
            Some(ArtifactPresence::Missing) | None => {}
        }
    }
}

#[cfg(test)]
mod tests;
