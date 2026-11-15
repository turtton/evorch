//! Desktop feature of the `evaluation` area, read side (PR 13): the evaluation
//! groups on disk, one group opened for its members, and a member's report. PR
//! 17c adds the jobs (run, report, rejudge) to this file.
//!
//! A file that is missing, one that cannot be used and one that is there are shown
//! as three different things; the core guesses none of them. Nothing here shows a
//! path.
use std::collections::BTreeMap;

use super::diagnostics::read::{Phase, Read, phase_line};
use super::layout::{bounded_list, paged_list};
use super::{Availability, Feature, FeatureContext, FeatureId, FeatureReply};
use workbench_protocol::{
    events::UiEvent,
    handshake::Capability,
    ids::{EvaluationGroupId, EvaluationMemberId, PageCursor},
    management::{ArtifactPresence, EvaluationArtifactRef},
    namespaces::evaluation::{
        EvaluationEvent, EvaluationGroupDetail, EvaluationGroupPhase, EvaluationGroupSummary,
        EvaluationMemberPhase, EvaluationProblemKind, EvaluationQuery, EvaluationReply,
        EvaluationReportText, GetEvaluation, GetEvaluationReport, ListEvaluations,
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

fn problem_label(kind: EvaluationProblemKind) -> &'static str {
    match kind {
        EvaluationProblemKind::Malformed => "形式が壊れています",
        EvaluationProblemKind::UnsupportedSchema => "未対応の版です",
        EvaluationProblemKind::TooLarge => "大きすぎます",
        EvaluationProblemKind::PermissionDenied => "読み取りが許可されていません",
        EvaluationProblemKind::UnsafePath => "安全でないパスです",
        EvaluationProblemKind::Io => "読み取りに失敗しました",
    }
}

fn group_phase_label(phase: EvaluationGroupPhase) -> &'static str {
    match phase {
        EvaluationGroupPhase::Running => "実行中",
        EvaluationGroupPhase::Finished => "完了",
        EvaluationGroupPhase::Incomplete => "未完了 (失敗またはレポートなしのメンバーあり)",
        EvaluationGroupPhase::Unverified => "状態を確認できません",
        EvaluationGroupPhase::Partial => "一部のメンバーのみ読み取り",
    }
}

fn member_phase_label(phase: EvaluationMemberPhase) -> &'static str {
    match phase {
        EvaluationMemberPhase::Starting => "開始中",
        EvaluationMemberPhase::RunningTrials => "試行中",
        EvaluationMemberPhase::Reporting => "レポート作成中",
        EvaluationMemberPhase::Finished => "完了",
        EvaluationMemberPhase::FinishedWithoutReport => "完了 (レポートなし)",
        EvaluationMemberPhase::Failed => "失敗",
        EvaluationMemberPhase::Unverified => "状態を確認できません",
    }
}

/// How an artifact stands: three states, never merged.
fn presence_label(artifact: &EvaluationArtifactRef) -> &'static str {
    match artifact.presence {
        ArtifactPresence::Present => "あり",
        ArtifactPresence::Missing => "なし",
        ArtifactPresence::Problem => "利用不可",
    }
}

fn group_row(group: &EvaluationGroupSummary) -> String {
    match group.presence {
        ArtifactPresence::Present => format!(
            "{} · {} · {} · メンバー {}",
            group.group.as_str(),
            group.phase.map_or("", group_phase_label),
            group.started_at.as_deref().unwrap_or(""),
            group.members_total.unwrap_or(0)
        ),
        ArtifactPresence::Missing => format!("{} · 記録なし", group.group.as_str()),
        ArtifactPresence::Problem => format!(
            "{} · 読み取れません ({})",
            group.group.as_str(),
            group.problem.map_or("", problem_label)
        ),
    }
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

    fn begin_group(&mut self, ctx: &mut FeatureContext<'_>, group: EvaluationGroupId) {
        self.group = None;
        self.member = None;
        self.report = None;
        self.report_read.forget();
        self.report_request = None;
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
        "Evaluation"
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
        if ui.button("再読み込み").clicked() {
            self.list_due = true;
            self.group_due = self.selected.is_some();
            self.report_due = self.member.is_some();
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

        if let Some(line) = phase_line(self.list_read.phase()) {
            ui.label(line);
        }
        if self.list_read.phase() == &Phase::Ready && self.groups.is_empty() {
            ui.label("評価グループはありません。");
        }
        if let Some(problem) = self.list_problem {
            ui.label(format!(
                "一覧を読み取れませんでした ({})。",
                problem_label(problem)
            ));
        }
        if self.omitted > 0 || self.scan_truncated {
            ui.label(format!(
                "ほかに {} 件のグループがあります(表示は新しい順の一部)。",
                self.omitted
            ));
        }
        let mut open = None;
        let groups = &self.groups;
        let more = paged_list(
            ui,
            "evaluation-groups",
            self.next_cursor.is_some().then_some("さらに読み込む"),
            |ui| {
                for group in groups {
                    if ui.button(group_row(group)).clicked() {
                        open = Some(group.group.clone());
                    }
                }
            },
        );
        if more {
            self.more_requested = true;
        }
        if let Some(group) = open {
            self.selected = Some(group);
            self.group_due = true;
        }

        if self.selected.is_some() {
            ui.separator();
            if let Some(line) = phase_line(self.group_read.phase()) {
                ui.label(line);
            }
        }
        let mut open_member = None;
        if let Some(group) = &self.group {
            ui.strong(format!(
                "{} · {} · 開始 {}",
                group.group.as_str(),
                group_phase_label(group.phase),
                group.started_at
            ));
            if let Some(finished) = &group.finished_at {
                ui.label(format!("終了 {finished}"));
            }
            if let Some(notice) = &group.notice {
                ui.label(format!("通知: {notice}"));
            }
            if group.members_omitted > 0 {
                ui.label(format!(
                    "メンバー {} 件のうち {} 件は表示していません。",
                    group.members_total, group.members_omitted
                ));
            }
            bounded_list(ui, "evaluation-members", |ui| {
                for member in &group.members {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(format!(
                            "{} · {} · 試行 完了 {} / 失敗 {} / 実行中 {} / 予定 {}",
                            member.member.as_str(),
                            member_phase_label(member.phase),
                            member.trials.finished,
                            member.trials.failed,
                            member.trials.running,
                            member.trials.planned
                        ));
                        ui.label(format!(
                            "レポート: {} / リプレイ: {}",
                            presence_label(&member.report),
                            presence_label(&member.replay)
                        ));
                        if let Some(failure) = &member.failure {
                            ui.label(format!("失敗: {failure}"));
                        }
                        if ui
                            .button(format!("{} のレポートを開く", member.member.as_str()))
                            .clicked()
                        {
                            open_member = Some(member.member.clone());
                        }
                    });
                }
            });
        }
        if let Some(member) = open_member {
            self.member = Some(member);
            self.report_due = true;
        }

        if self.member.is_some() {
            ui.separator();
            if let Some(line) = phase_line(self.report_read.phase()) {
                ui.label(line);
            }
            if let Some(report) = &self.report {
                match (report.artifact.presence, &report.text, report.problem) {
                    (ArtifactPresence::Present, Some(text), _) => {
                        ui.strong(format!("{} のレポート", report.artifact.member.as_str()));
                        ui.label(&text.text);
                        if text.truncated {
                            ui.label(format!(
                                "(先頭 {} バイトのみ表示 / 全体 {} バイト)",
                                text.text.len(),
                                text.total_bytes
                            ));
                        }
                    }
                    (ArtifactPresence::Problem, _, problem) => {
                        ui.label(format!(
                            "レポートは利用できません ({})。",
                            problem.map_or("", problem_label)
                        ));
                    }
                    _ => {
                        ui.label("レポートはまだありません。");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
