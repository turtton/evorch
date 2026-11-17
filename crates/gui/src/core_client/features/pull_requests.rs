//! Desktop feature of the `pull_requests` area (PR 15): the pull request the
//! selected thread tracks, as the core holds it, and the commands that change
//! it (link, unlink, refresh, candidates, the automatic fix policy and budget,
//! the manual notice).
//!
//! The screen never runs `gh` and never opens the database. It sends commands
//! that quote the revisions it read, so a link or a policy change made from
//! another client in the meantime is refused by the core (`stale_revision`) and
//! the screen reads the thread again. A command whose outcome the client could
//! not see (the reply was lost, or the core says `outcome_unknown`) is never
//! shown as done: the operation is asked for, and one the core no longer knows
//! stays "unknown" until the thread is read again.
//!
//! As in the other areas an event only says something changed; the thread is
//! then asked for again. Whether the automatic fix budget allows a notice, and
//! whether the person is told of its limit, is the core's decision: this screen
//! shows `AutoFixLimitReached` as it arrives and decides nothing.
use std::collections::{BTreeMap, BTreeSet};

use super::layout::bounded_list;
use super::{Availability, Feature, FeatureContext, FeatureId, FeatureReply};
use workbench_protocol::{
    commands::{Command, CommandResult},
    errors::{ErrorCode, RetryAdvice},
    events::UiEvent,
    handshake::Capability,
    ids::{OperationId, ThreadId},
    namespaces::pull_requests::{
        CandidateState, ChangeAutoFixDestination, ConfirmCandidate, DismissCandidate, FactsView,
        HeadAttribution, LinkPullRequest, PullRequestOperationKind, PullRequestOperationView,
        PullRequestsCommand, PullRequestsEvent, PullRequestsQuery, PullRequestsReply,
        PullRequestsResult, RefreshPullRequest, RequestManualNotice, ResetAutoFixBudget,
        SetAutoFix, ThreadPullRequest, UnlinkPullRequest,
    },
    operation::{CancelReason, OperationState, OperationView},
    queries::{Query, QueryReply},
    wire::Revision,
};

#[derive(Debug, Clone)]
enum Pending {
    Thread(ThreadId),
    Operation(OperationId),
    Command(OperationId),
}

#[derive(Default)]
pub struct PullRequestsFeature {
    next_tag: u64,
    pending: BTreeMap<u64, Pending>,
    thread: Option<ThreadId>,
    view: Option<ThreadPullRequest>,
    /// The operations this screen started, running or finished.
    operations: BTreeMap<OperationId, PullRequestOperationView>,
    /// Operations whose end is to be asked for.
    to_check: BTreeSet<OperationId>,
    /// Operations the core no longer knows while their end was not seen: their
    /// result is unknown, and they no longer keep the screen busy.
    lost: BTreeSet<OperationId>,
    locator: String,
    status: String,
    /// What the core told the person about (limit stops, merged, ...), newest last.
    notices: Vec<String>,
    stale: bool,
    in_flight: bool,
    /// A move of the notice destination the person has not confirmed yet.
    destination_move: Option<DestinationMove>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DestinationChange {
    ResetBudget,
    ChangeDestination,
}

/// Taking the notices over from another thread moves them away from it, so
/// the person confirms the exact thread they leave before the command is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DestinationMove {
    thread: ThreadId,
    change: DestinationChange,
    previous: ThreadId,
}

const MAX_NOTICES: usize = 16;

fn new_operation() -> OperationId {
    OperationId::new(format!("gui-pr-{}", uuid::Uuid::new_v4())).expect("a uuid fits")
}

fn failure_label(code: ErrorCode, unknown: bool) -> &'static str {
    if unknown {
        return "操作の結果を確認できませんでした。最新の状態を読み込み直して確認してください。";
    }
    match code {
        ErrorCode::StaleRevision => {
            "他の場所で変更されました。最新の状態を読み込みました。もう一度操作してください。"
        }
        ErrorCode::PermissionDenied => "このコアはこのスレッドを保持していないため操作できません。",
        ErrorCode::InvalidRequest => "PR の指定が不正です（URL か owner/repo#番号）。",
        ErrorCode::NotFound => "対象が見つかりません。",
        ErrorCode::Conflict => "現在の状態では実行できません。",
        ErrorCode::UnsupportedCapability => "PR の追跡は無効です。",
        ErrorCode::Draining => "コアが停止または再起動中です。",
        ErrorCode::LimitExceeded => limit_label(None),
        _ => "操作に失敗しました。",
    }
}

/// A refusal for a limit, as the person reads it. The core says which limit by its
/// advice: `Never` is the memory of accepted commands being full, which only a
/// restart of the core empties; `UserActionRequired` is the table of running
/// operations being full, which clears as they finish. Neither is sent again by
/// the screen.
fn limit_label(retry: Option<RetryAdvice>) -> &'static str {
    match retry {
        Some(RetryAdvice::Never) => {
            "このコアが受け付け済みの操作の記憶が上限に達したため、新しい操作は受け付けられません。コアを再起動すると解消します。操作は再送されません。"
        }
        Some(RetryAdvice::UserActionRequired) => {
            "実行中の操作が多すぎます。実行中の操作が終わってから、もう一度操作してください。操作は自動では再送されません。"
        }
        _ => "上限に達したため操作を受け付けられませんでした。操作は再送されません。",
    }
}

fn success_label(kind: PullRequestOperationKind) -> &'static str {
    match kind {
        PullRequestOperationKind::Link | PullRequestOperationKind::ConfirmCandidate => {
            "PR をリンクしました。"
        }
        PullRequestOperationKind::Unlink => "リンクを解除しました。",
        PullRequestOperationKind::Refresh => "",
        PullRequestOperationKind::DismissCandidate => "候補を無視しました。",
        PullRequestOperationKind::SetAutoFix
        | PullRequestOperationKind::ResetAutoFixBudget
        | PullRequestOperationKind::ChangeAutoFixDestination => "自動修正の設定を変更しました。",
        PullRequestOperationKind::RequestManualNotice => "修正依頼を予約しました。",
    }
}

fn facts_lines(ui: &mut egui::Ui, facts: &FactsView) {
    ui.label(format!(
        "状態 {:?} / ベース {} / ヘッド {} ({})",
        facts.state,
        facts.base_branch,
        facts.head_branch,
        facts.head_sha.chars().take(10).collect::<String>()
    ));
    ui.label(format!("レビュー {:?}", facts.review_decision));
    let failing = facts
        .checks
        .iter()
        .filter(|check| {
            matches!(
                check.conclusion.as_deref(),
                Some("failure" | "timed_out" | "cancelled" | "action_required")
            )
        })
        .count();
    ui.label(format!(
        "チェック {} 件（失敗 {} 件{}）",
        facts.checks.len(),
        failing,
        if facts.checks_complete {
            ""
        } else {
            "・一部のみ"
        }
    ));
}

impl PullRequestsFeature {
    fn tag(&mut self, pending: Pending) -> u64 {
        self.next_tag += 1;
        self.pending.insert(self.next_tag, pending);
        self.next_tag
    }

    fn running(&self) -> bool {
        self.operations
            .iter()
            .any(|(id, view)| !view.operation.state.is_final() && !self.lost.contains(id))
    }

    fn switch_thread(&mut self, thread: Option<ThreadId>) {
        self.stale = thread.is_some();
        self.thread = thread;
        self.view = None;
        self.locator.clear();
        self.status.clear();
        self.notices.clear();
        // Replies of the previous thread are dropped by their tag.
        self.pending
            .retain(|_, pending| matches!(pending, Pending::Command(_)));
        self.in_flight = false;
    }

    fn operation_lost(&mut self, id: &OperationId) {
        if !self.operations.contains_key(id) {
            return;
        }
        self.lost.insert(id.clone());
        self.stale = true;
        self.status = failure_label(ErrorCode::Internal, true).into();
    }

    fn operation_update(&mut self, view: PullRequestOperationView) {
        let id = view.operation.operation_id.clone();
        let Some(known) = self.operations.get(&id) else {
            return;
        };
        // An end is final: an older reply arriving after the completion
        // (still `Pending`) does not make the screen busy again.
        if known.operation.state.is_final() && !view.operation.state.is_final() {
            return;
        }
        let state = view.operation.state;
        let kind = view.kind;
        let message = view.message.clone();
        let unknown = view.outcome_unknown;
        self.operations.insert(id.clone(), view);
        self.lost.remove(&id);
        match state {
            OperationState::Pending => {}
            OperationState::Ready => {
                self.status = success_label(kind).into();
                self.stale = true;
            }
            OperationState::Failed { code } => {
                self.status = match message {
                    Some(message) if code != ErrorCode::StaleRevision && !unknown => {
                        format!("{} {message}", failure_label(code, false))
                    }
                    _ => failure_label(code, unknown).to_owned(),
                };
                self.stale = true;
            }
            OperationState::Cancelled { reason } => {
                self.status = match reason {
                    CancelReason::Requested => "操作を取り消しました。",
                    CancelReason::Restart | CancelReason::Shutdown => {
                        "再起動または終了のため操作は行われませんでした。"
                    }
                }
                .into();
                self.stale = true;
            }
        }
    }

    fn notice(&mut self, text: String) {
        if self.notices.len() >= MAX_NOTICES {
            self.notices.remove(0);
        }
        self.notices.push(text);
    }

    fn send(&mut self, ctx: &mut FeatureContext<'_>, command: PullRequestsCommand) {
        let id = command.operation_id().clone();
        self.operations.insert(
            id.clone(),
            PullRequestOperationView {
                operation: OperationView {
                    operation_id: id.clone(),
                    state: OperationState::Pending,
                    revision: Revision(0),
                    result: None,
                },
                kind: command.kind(),
                thread: command.thread().clone(),
                lifecycle_revision: None,
                policy_revision: None,
                notice_id: None,
                message: None,
                outcome_unknown: false,
            },
        );
        let tag = self.tag(Pending::Command(id));
        ctx.command(tag, Command::PullRequests(command));
    }
}

impl Feature for PullRequestsFeature {
    fn id(&self) -> FeatureId {
        FeatureId::PullRequests
    }

    fn title(&self) -> &'static str {
        "Pull requests"
    }

    fn availability(&self) -> Availability {
        Availability::Implemented {
            required: &[Capability::Commands, Capability::Queries],
        }
    }

    fn reset(&mut self) {
        self.view = None;
        self.in_flight = false;
        self.stale = self.thread.is_some();
        self.pending
            .retain(|_, pending| matches!(pending, Pending::Command(_)));
        for (id, view) in &self.operations {
            if !view.operation.state.is_final() && !self.lost.contains(id) {
                self.to_check.insert(id.clone());
            }
        }
    }

    fn event(&mut self, event: &UiEvent) {
        let UiEvent::PullRequests(event) = event else {
            return;
        };
        match event {
            PullRequestsEvent::PullRequestChanged {
                thread,
                notifications,
                ..
            } => {
                if self.thread.as_ref() == Some(thread) {
                    self.stale = true;
                    for kind in notifications {
                        self.notice(format!("通知: {kind}"));
                    }
                }
            }
            PullRequestsEvent::AutoFixLimitReached { thread, limit, .. } => {
                if self.thread.as_ref() == Some(thread) {
                    self.stale = true;
                    self.notice(format!(
                        "自動修正の上限（{limit} 回）に達したため停止しました。再開するには予算をリセットしてください。"
                    ));
                }
            }
            PullRequestsEvent::PullRequestOperationCompleted { operation } => {
                self.operation_update(operation.clone());
            }
        }
    }

    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        let Some(pending) = self.pending.remove(&tag) else {
            return;
        };
        match (pending, reply) {
            (Pending::Thread(thread), FeatureReply::Query(result)) => {
                self.in_flight = false;
                match result {
                    Ok(QueryReply::PullRequests(PullRequestsReply::Thread(view)))
                        if self.thread.as_ref() == Some(&thread) && view.thread == thread =>
                    {
                        self.view = Some(*view);
                    }
                    Ok(_) => {}
                    Err(error) => {
                        self.status = error.message;
                    }
                }
            }
            (Pending::Operation(id), FeatureReply::Query(result)) => match result {
                Ok(QueryReply::PullRequests(PullRequestsReply::Operation(view))) => {
                    self.operation_update(view);
                }
                Ok(_) => {}
                Err(error) if error.code == Some(ErrorCode::NotFound) => self.operation_lost(&id),
                Err(error) => self.status = error.message,
            },
            (Pending::Command(id), FeatureReply::Command(result)) => match result {
                Ok(reply) => {
                    if let Some(CommandResult::PullRequests(PullRequestsResult::Operation(view))) =
                        reply.result.map(|boxed| *boxed)
                    {
                        self.operation_update(view);
                    }
                }
                Err(error) => {
                    if error.outcome_unknown {
                        // The command may have been applied: its operation is
                        // asked for, never sent again under a new id and never
                        // shown as done.
                        self.status = failure_label(ErrorCode::Internal, true).into();
                        self.to_check.insert(id);
                    } else {
                        self.status = match error.code {
                            Some(ErrorCode::LimitExceeded) => limit_label(error.retry).to_owned(),
                            Some(code) => failure_label(code, false).to_owned(),
                            None => error.message.clone(),
                        };
                        self.operations.remove(&id);
                        self.stale = true;
                    }
                }
            },
            _ => {}
        }
    }

    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        let selected = ctx.selection().thread.clone();
        if selected != self.thread {
            self.switch_thread(selected);
        }
        let Some(thread) = self.thread.clone() else {
            ui.label("スレッドを選択してください。");
            return;
        };
        if self.stale && !self.in_flight {
            self.stale = false;
            self.in_flight = true;
            let tag = self.tag(Pending::Thread(thread.clone()));
            ctx.query(
                tag,
                Query::PullRequests(PullRequestsQuery::GetThreadPullRequest {
                    thread: thread.clone(),
                }),
            );
        }
        for id in std::mem::take(&mut self.to_check) {
            let tag = self.tag(Pending::Operation(id.clone()));
            ctx.query(
                tag,
                Query::PullRequests(PullRequestsQuery::GetPullRequestOperation {
                    operation_id: id,
                }),
            );
        }
        if !self.status.is_empty() {
            ui.label(&self.status);
        }
        for notice in &self.notices {
            ui.colored_label(ui.visuals().warn_fg_color, notice);
        }
        let Some(view) = self.view.clone() else {
            ui.label("PR の状態を読み込んでいます。");
            return;
        };
        if !view.tracking_enabled {
            ui.label("PR の追跡は無効です（[pull_requests] enabled）。");
            return;
        }
        let busy = self.running();
        let can_write = view.owned_here && !busy;
        if !view.owned_here {
            ui.label("このコアはこのスレッドを保持していないため、閲覧のみです。");
        }
        if busy {
            ui.label("実行中…");
        }
        let mut commands = Vec::new();
        match &view.link {
            Some(link) => {
                let reference = &link.pull_request;
                ui.strong(format!(
                    "{}/{}#{}",
                    reference.host, reference.repository, reference.number
                ));
                if let Some(snapshot) = &view.snapshot {
                    if let Some(title) = &snapshot.title {
                        ui.label(title);
                    }
                    if let Some(facts) = &snapshot.facts {
                        facts_lines(ui, facts);
                    }
                    if let Some(failure) = &snapshot.last_failure {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            format!(
                                "取得に失敗しました（{}）: {}",
                                failure.kind, failure.message
                            ),
                        );
                    }
                }
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(can_write, egui::Button::new("更新"))
                        .clicked()
                    {
                        commands.push(PullRequestsCommand::RefreshPullRequest(
                            RefreshPullRequest {
                                operation_id: new_operation(),
                                thread: thread.clone(),
                            },
                        ));
                    }
                    if ui
                        .add_enabled(can_write, egui::Button::new("リンク解除"))
                        .clicked()
                    {
                        commands.push(PullRequestsCommand::UnlinkPullRequest(UnlinkPullRequest {
                            operation_id: new_operation(),
                            thread: thread.clone(),
                            expected_revision: view.lifecycle_revision,
                        }));
                    }
                });
                if view.head_attribution == HeadAttribution::Unknown && view.local_head.is_some() {
                    ui.label("ローカルのヘッドの由来は不明です。");
                }
                if let Some(auto_fix) = &view.auto_fix {
                    ui.separator();
                    ui.strong("自動修正");
                    ui.label(format!(
                        "{} / 使用 {}/{}（予約 {}）{}",
                        if auto_fix.enabled { "有効" } else { "無効" },
                        auto_fix.used,
                        auto_fix.limit,
                        auto_fix.reserved,
                        if auto_fix.globally_enabled {
                            ""
                        } else {
                            " / コア全体では無効"
                        }
                    ));
                    if let Some(destination) = &auto_fix.destination {
                        ui.label(format!("通知先: {destination}"));
                    }
                    if auto_fix.limit_reached {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            "上限に達しました。再開には予算のリセットが必要です。",
                        );
                    } else if let Some(reason) = &auto_fix.stop_reason {
                        ui.label(format!("自動修正の停止理由: {reason}"));
                    }
                    let link_revision = link.link_revision;
                    let policy_revision = auto_fix.policy_revision;
                    let previous = auto_fix
                        .destination
                        .clone()
                        .filter(|destination| *destination != thread);
                    // A confirmation is only good for the destination it named.
                    if self.destination_move.as_ref().is_some_and(|pending| {
                        pending.thread != thread || Some(&pending.previous) != previous.as_ref()
                    }) {
                        self.destination_move = None;
                    }
                    ui.horizontal_wrapped(|ui| {
                        let toggle = if auto_fix.enabled {
                            "無効にする"
                        } else {
                            "有効にする"
                        };
                        if ui
                            .add_enabled(can_write, egui::Button::new(toggle))
                            .clicked()
                        {
                            commands.push(PullRequestsCommand::SetAutoFix(SetAutoFix {
                                operation_id: new_operation(),
                                thread: thread.clone(),
                                link_revision,
                                policy_revision,
                                enabled: !auto_fix.enabled,
                            }));
                        }
                        if ui
                            .add_enabled(can_write, egui::Button::new("予算をリセット"))
                            .clicked()
                        {
                            match &previous {
                                // Nothing is moved: no confirmation to give.
                                None => commands.push(PullRequestsCommand::ResetAutoFixBudget(
                                    ResetAutoFixBudget {
                                        operation_id: new_operation(),
                                        thread: thread.clone(),
                                        link_revision,
                                        policy_revision,
                                        confirmed_previous: None,
                                    },
                                )),
                                Some(previous) => {
                                    self.destination_move = Some(DestinationMove {
                                        thread: thread.clone(),
                                        change: DestinationChange::ResetBudget,
                                        previous: previous.clone(),
                                    });
                                }
                            }
                        }
                        if ui
                            .add_enabled(
                                can_write && auto_fix.destination.as_ref() != Some(&thread),
                                egui::Button::new("通知先をこのスレッドにする"),
                            )
                            .clicked()
                        {
                            match &previous {
                                None => {
                                    commands.push(PullRequestsCommand::ChangeAutoFixDestination(
                                        ChangeAutoFixDestination {
                                            operation_id: new_operation(),
                                            thread: thread.clone(),
                                            link_revision,
                                            policy_revision,
                                            confirmed_previous: None,
                                        },
                                    ))
                                }
                                Some(previous) => {
                                    self.destination_move = Some(DestinationMove {
                                        thread: thread.clone(),
                                        change: DestinationChange::ChangeDestination,
                                        previous: previous.clone(),
                                    });
                                }
                            }
                        }
                        let head = view
                            .snapshot
                            .as_ref()
                            .and_then(|snapshot| snapshot.facts.as_ref())
                            .map(|facts| facts.head_sha.clone());
                        if ui
                            .add_enabled(
                                can_write && head.is_some(),
                                egui::Button::new("今すぐ修正を依頼"),
                            )
                            .clicked()
                            && let Some(expected_head) = head
                        {
                            commands.push(PullRequestsCommand::RequestManualNotice(
                                RequestManualNotice {
                                    operation_id: new_operation(),
                                    thread: thread.clone(),
                                    link_revision,
                                    expected_head,
                                },
                            ));
                        }
                    });
                    if let Some(pending) = self.destination_move.clone() {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            format!(
                                "自動修正の通知先を {} からこのスレッドへ移します。よろしいですか。",
                                pending.previous
                            ),
                        );
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(can_write, egui::Button::new("移す"))
                                .clicked()
                            {
                                let confirmed_previous = Some(pending.previous.clone());
                                commands.push(match pending.change {
                                    DestinationChange::ResetBudget => {
                                        PullRequestsCommand::ResetAutoFixBudget(
                                            ResetAutoFixBudget {
                                                operation_id: new_operation(),
                                                thread: thread.clone(),
                                                link_revision,
                                                policy_revision,
                                                confirmed_previous,
                                            },
                                        )
                                    }
                                    DestinationChange::ChangeDestination => {
                                        PullRequestsCommand::ChangeAutoFixDestination(
                                            ChangeAutoFixDestination {
                                                operation_id: new_operation(),
                                                thread: thread.clone(),
                                                link_revision,
                                                policy_revision,
                                                confirmed_previous,
                                            },
                                        )
                                    }
                                });
                                self.destination_move = None;
                            }
                            if ui.button("やめる").clicked() {
                                self.destination_move = None;
                            }
                        });
                    }
                }
            }
            None => {
                ui.label("このスレッドにリンクされた PR はありません。");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.locator)
                            .hint_text("URL または owner/repo#番号")
                            .desired_width(280.0),
                    );
                    let locator = self.locator.trim().to_owned();
                    if ui
                        .add_enabled(
                            can_write && !locator.is_empty(),
                            egui::Button::new("リンク"),
                        )
                        .clicked()
                    {
                        commands.push(PullRequestsCommand::LinkPullRequest(LinkPullRequest {
                            operation_id: new_operation(),
                            thread: thread.clone(),
                            locator,
                            expected_revision: view.lifecycle_revision,
                        }));
                        self.locator.clear();
                    }
                });
            }
        }
        let candidates: Vec<_> = view
            .candidates
            .iter()
            .filter(|candidate| candidate.state == CandidateState::Presented)
            .collect();
        if !candidates.is_empty() {
            ui.separator();
            ui.strong("候補");
            bounded_list(ui, "pull-request-candidates", |ui| {
                for candidate in candidates {
                    let reference = &candidate.pull_request;
                    ui.push_id(candidate.candidate_id, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(format!(
                                "{}/{}#{}",
                                reference.host, reference.repository, reference.number
                            ));
                            if ui
                                .add_enabled(can_write, egui::Button::new("リンクする"))
                                .clicked()
                            {
                                commands.push(PullRequestsCommand::ConfirmPullRequestCandidate(
                                    ConfirmCandidate {
                                        operation_id: new_operation(),
                                        thread: thread.clone(),
                                        candidate_id: candidate.candidate_id,
                                        expected_revision: view.lifecycle_revision,
                                    },
                                ));
                            }
                            if ui
                                .add_enabled(can_write, egui::Button::new("無視"))
                                .clicked()
                            {
                                commands.push(PullRequestsCommand::DismissPullRequestCandidate(
                                    DismissCandidate {
                                        operation_id: new_operation(),
                                        thread: thread.clone(),
                                        candidate_id: candidate.candidate_id,
                                    },
                                ));
                            }
                        });
                    });
                }
            });
        }
        for command in commands {
            self.send(ctx, command);
        }
    }
}

#[cfg(test)]
mod tests;
