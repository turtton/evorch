//! Desktop feature of the `lanes` area: the lanes the selected thread leads,
//! one lane opened for its detail, and the human operations on a lane: set its
//! policy, detach it, reattach it, deliver again the notices held for its
//! lead, and end it by a person's decision (close it as done, unverified, or
//! discard it; see `close`).
//!
//! The lane ledger is the truth. Lists and details are read from the core; an
//! event only names a lane that changed and the reads are repeated, so a late
//! event never moves the screen back. Every operation names the management
//! revision the screen showed, and the lead's current root run where one is
//! needed; the core refuses an older one. Continuing a lane has no gate here.
use std::collections::BTreeMap;

mod close;

use super::layout::bounded_list;
use super::{
    Availability, Feature, FeatureContext, FeatureError, FeatureId, FeatureReply, Navigation,
};
use workbench_protocol::{
    commands::{Command, CommandResult},
    errors::ErrorCode,
    events::UiEvent,
    handshake::Capability,
    ids::{PageCursor, RunId, ThreadId},
    lane_attention::LaneId,
    lane_management::{
        LaneManagementAction, LaneManagementOperationId, LaneManagementPolicy,
        LaneManagementRequest, LaneManagementState, ManageLane, ManagedLanePolicy,
    },
    namespaces::{
        ExecutionQuery, ExecutionReply, LanesCommand, LanesEvent, LanesQuery, LanesReply,
        LanesResult,
        execution::{RunDetailsQuery, RunScope},
        lanes::{
            LaneDetail, LaneReportStatus, LaneStartState, LaneSummary, ListLanes,
            ResumeLaneDeliveries,
        },
    },
    queries::{PageRequest, Query, QueryReply},
    wire::Revision,
};

#[derive(Debug, Clone)]
enum Pending {
    List {
        thread: ThreadId,
        append: bool,
    },
    Detail {
        lane: LaneId,
    },
    LeadRoot {
        thread: ThreadId,
    },
    Detached {
        thread: ThreadId,
        append: bool,
    },
    Management {
        lane: LaneId,
    },
    Command(&'static str),
    /// The read of the close preview the open dialog waits for (its `read_seq`).
    ClosePreview {
        lane: LaneId,
        seq: u64,
    },
    /// A close or discard that was sent; at most one is in flight.
    CloseCommand,
}

/// A reattach waits for the lane's management state (its family revision) and
/// the lead's current root run before it is sent.
struct Reattach {
    lane: LaneId,
    state: Option<LaneManagementState>,
}

#[derive(Default)]
pub struct LanesFeature {
    next_tag: u64,
    pending: BTreeMap<u64, Pending>,
    thread: Option<ThreadId>,
    lanes: Vec<LaneSummary>,
    next_cursor: Option<PageCursor>,
    opened: Option<LaneId>,
    detail: Option<LaneDetail>,
    /// The lead's latest root run, as the core last listed it.
    lead_root: Option<RunId>,
    lead_root_next: Option<PageCursor>,
    /// Lanes opened from this thread that no lead manages now, as the core lists them. A
    /// detached lane is not in the lead's list, and is offered here for reattaching.
    detached: Vec<LaneSummary>,
    detached_next: Option<PageCursor>,
    /// Pages of detached lanes loaded now, and how many the screen shows or has asked for: a
    /// refresh re-reads the pages the user had loaded, instead of dropping all but the first.
    detached_pages: usize,
    detached_target_pages: usize,
    reattach: Option<Reattach>,
    /// The confirmation for closing or discarding a lane, while it is open.
    close: Option<close::CloseDialog>,
    status: String,
    list_stale: bool,
    detached_stale: bool,
    detail_stale: bool,
    root_stale: bool,
    more_requested: bool,
    detached_more_requested: bool,
}

impl LanesFeature {
    fn tag(&mut self, pending: Pending) -> u64 {
        self.next_tag += 1;
        self.pending.insert(self.next_tag, pending);
        self.next_tag
    }

    fn switch_thread(&mut self, thread: Option<ThreadId>) {
        self.thread = thread;
        self.lanes.clear();
        self.next_cursor = None;
        self.opened = None;
        self.detail = None;
        self.lead_root = None;
        self.lead_root_next = None;
        self.detached.clear();
        self.detached_next = None;
        self.detached_pages = 0;
        self.detached_target_pages = 0;
        self.reattach = None;
        self.close = None;
        self.status.clear();
        self.list_stale = self.thread.is_some();
        self.detached_stale = self.thread.is_some();
        self.root_stale = self.thread.is_some();
    }

    /// The lead's current root has been read: no root query is in flight or due.
    fn root_settled(&self) -> bool {
        !self.root_stale
            && self.lead_root_next.is_none()
            && !self
                .pending
                .values()
                .any(|pending| matches!(pending, Pending::LeadRoot { .. }))
    }

    fn command(&mut self, ctx: &mut FeatureContext<'_>, label: &'static str, command: Command) {
        let tag = self.tag(Pending::Command(label));
        ctx.command(tag, command);
    }

    fn manage(
        &mut self,
        ctx: &mut FeatureContext<'_>,
        label: &'static str,
        lane: &LaneId,
        revision: Revision,
        action: LaneManagementAction,
        family: Option<workbench_protocol::lane_management::LaneFamilyRevision>,
    ) {
        let operation = format!("gui-lane-{}", uuid::Uuid::new_v4());
        let Ok(operation_id) = LaneManagementOperationId::new(operation) else {
            return;
        };
        self.command(
            ctx,
            label,
            Command::Lanes(LanesCommand::ManageLane(ManageLane {
                request: LaneManagementRequest {
                    operation_id,
                    lane_id: lane.clone(),
                    expected_management_revision: revision,
                    action,
                },
                expected_family_revision: family,
            })),
        );
    }

    fn refresh_after_command(&mut self) {
        self.list_stale = true;
        self.detached_stale = true;
        self.detail_stale = self.opened.is_some();
        self.root_stale = true;
    }

    /// Reads what the open confirmation needs, draws it and runs its answer.
    fn drive_close(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        if let Some(dialog) = &mut self.close
            && dialog.read_due
        {
            dialog.read_due = false;
            dialog.read_seq += 1;
            dialog.phase = close::ClosePhase::Loading;
            dialog.stale = false;
            let (lane, seq) = (dialog.lane.clone(), dialog.read_seq);
            let tag = self.tag(Pending::ClosePreview {
                lane: lane.clone(),
                seq,
            });
            ctx.query(
                tag,
                Query::Lanes(LanesQuery::GetLaneClosePreview { lane_id: lane }),
            );
        }
        let sending = self
            .pending
            .values()
            .any(|pending| matches!(pending, Pending::CloseCommand));
        let Some(dialog) = &mut self.close else {
            return;
        };
        match close::draw(ui.ctx(), dialog, sending) {
            Some(close::DialogAction::Confirm) => {
                if let Some(order) = dialog.order(sending) {
                    let tag = self.tag(Pending::CloseCommand);
                    ctx.command(tag, Command::Lanes(LanesCommand::CloseLane(order)));
                }
            }
            Some(close::DialogAction::Reload) => {
                dialog.read_due = true;
                dialog.notice = None;
            }
            Some(close::DialogAction::Cancel) => self.close = None,
            None => {}
        }
    }

    fn drive_reattach(&mut self, ctx: &mut FeatureContext<'_>) {
        if !self.root_settled() {
            return;
        }
        let (Some(flow), Some(lead), Some(root)) =
            (&self.reattach, self.thread.clone(), self.lead_root.clone())
        else {
            return;
        };
        let Some(state) = flow.state.clone() else {
            return;
        };
        let lane = flow.lane.clone();
        self.reattach = None;
        self.manage(
            ctx,
            "lane の再接続",
            &lane,
            state.management_revision,
            LaneManagementAction::Reattach {
                lead_thread_id: lead,
                expected_lead_root_run_id: root,
            },
            state.family_revision,
        );
    }

    fn draw_detached_row(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &mut FeatureContext<'_>,
        detached: LaneSummary,
    ) {
        let lane = detached.lane_id;
        ui.push_id(("detached", lane.as_str()), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("切り離し済み: {}", detached.objective_preview));
                if self.reattach.is_none() && ui.button("再接続").clicked() {
                    self.reattach = Some(Reattach {
                        lane: lane.clone(),
                        state: None,
                    });
                    let tag = self.tag(Pending::Management { lane: lane.clone() });
                    ctx.query(
                        tag,
                        Query::Lanes(LanesQuery::GetLaneManagement {
                            lane_id: lane.clone(),
                        }),
                    );
                    self.lead_root = None;
                    self.root_stale = true;
                }
            });
        });
    }

    fn draw_lane_row(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &mut FeatureContext<'_>,
        lane: &LaneSummary,
    ) {
        ui.push_id(lane.lane_id.as_str(), |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button(&lane.objective_preview).clicked() {
                    self.opened = Some(lane.lane_id.clone());
                    self.detail = None;
                    self.detail_stale = true;
                }
                ui.label(policy_label(lane.policy));
                ui.label(start_label(lane.start));
                if let Some(status) = lane.latest_report_status {
                    ui.label(report_label(status));
                }
                let (toggle, target) = match lane.policy {
                    LaneManagementPolicy::Attached => {
                        ("概要のみにする", ManagedLanePolicy::Summary)
                    }
                    _ => ("添付にする", ManagedLanePolicy::Attached),
                };
                if lane.policy != LaneManagementPolicy::Detached && ui.button(toggle).clicked() {
                    self.manage(
                        ctx,
                        "lane の policy 変更",
                        &lane.lane_id,
                        lane.management_revision,
                        LaneManagementAction::SetPolicy { policy: target },
                        None,
                    );
                }
                if lane.policy != LaneManagementPolicy::Detached && ui.button("切り離す").clicked()
                {
                    self.manage(
                        ctx,
                        "lane の切り離し",
                        &lane.lane_id,
                        lane.management_revision,
                        LaneManagementAction::Detach,
                        None,
                    );
                }
                if lane.policy != LaneManagementPolicy::Detached {
                    for choice in [close::CloseChoice::Complete, close::CloseChoice::Discard] {
                        if ui.button(choice.button()).clicked() {
                            self.close = Some(close::CloseDialog::new(
                                lane.lane_id.clone(),
                                choice,
                                lane.objective_preview.clone(),
                            ));
                        }
                    }
                }
                if let Some(current) = &lane.current_close {
                    ui.label(close::badge(current));
                }
            });
        });
    }

    fn draw_detail(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        let Some(detail) = self.detail.clone() else {
            return;
        };
        ui.separator();
        ui.strong(format!("lane {}", detail.summary.lane_id.as_str()));
        ui.label(format!(
            "{} / 管理 revision {} / brief revision {} / 世代 {}",
            policy_label(detail.summary.policy),
            detail.summary.management_revision.0,
            detail.summary.brief_revision.0,
            detail.summary.generation.0
        ));
        if let Some(current) = &detail.summary.current_close {
            ui.label(close::badge(current));
        }
        if let Some(report) = &detail.latest_report {
            ui.label(format!(
                "最新報告 #{}: {} — {}{}",
                report.seq.0,
                report_label(report.status),
                report.summary,
                if report.needs_human {
                    "（人の判断待ち）"
                } else {
                    ""
                }
            ));
        }
        if let Some(ready) = &detail.ready {
            ui.label(format!(
                "Ready は brief revision {} に対する報告です{}（close を妨げません）",
                ready.brief_revision.0,
                if ready.brief_changed_since {
                    "。その後 brief が更新されています"
                } else {
                    ""
                }
            ));
        }
        bounded_list(ui, "lanes-attention", |ui| {
            for item in &detail.attention {
                ui.label(format!("要対応 {:?}: {}", item.tag, item.summary));
            }
        });
        if detail.delivery.held {
            ui.label(format!(
                "lead への通知が保留されています（{} 件）",
                detail.delivery.pending_count
            ));
            if let (Some(lead), Some(root)) = (self.thread.clone(), self.lead_root.clone())
                && detail.summary.lead_thread_id.as_ref() == Some(&lead)
                && ui.button("保留中の通知を再配信").clicked()
            {
                self.command(
                    ctx,
                    "通知の再配信",
                    Command::Lanes(LanesCommand::ResumeLaneDeliveries(ResumeLaneDeliveries {
                        lane_id: detail.summary.lane_id.clone(),
                        expected_management_revision: detail.summary.management_revision,
                        lead_thread_id: lead,
                        expected_lead_root_run_id: root,
                    })),
                );
            }
        }
        let project = ctx
            .cache()
            .threads
            .get(detail.summary.thread_id.as_str())
            .map(|thread| thread.project.clone());
        if let Some(project) = project
            && ui.button("lane の会話を開く").clicked()
        {
            ctx.navigate(Navigation::SelectThread {
                project,
                thread: detail.summary.thread_id.clone(),
            });
        }
    }
}

pub(crate) fn policy_label(policy: LaneManagementPolicy) -> &'static str {
    match policy {
        LaneManagementPolicy::Attached => "添付",
        LaneManagementPolicy::Summary => "概要",
        LaneManagementPolicy::Detached => "切り離し済み",
    }
}

fn start_label(start: LaneStartState) -> &'static str {
    match start {
        LaneStartState::NotStarted => "未開始",
        LaneStartState::StartBlocked => "開始不可",
        LaneStartState::OperatorStopped => "停止済み（再開可）",
        LaneStartState::Started => "開始済み",
    }
}

fn report_label(status: LaneReportStatus) -> &'static str {
    match status {
        LaneReportStatus::Working => "作業中",
        LaneReportStatus::Waiting => "待機中",
        LaneReportStatus::Blocked => "ブロック",
        LaneReportStatus::Ready => "Ready",
        LaneReportStatus::Failed => "失敗",
    }
}

fn failure(label: &str, error: &FeatureError) -> String {
    if error.outcome_unknown {
        format!("{label}: 結果不明（再送していません）— {}", error.message)
    } else {
        format!("{label}: {}", error.message)
    }
}

impl Feature for LanesFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Lanes
    }

    fn title(&self) -> &'static str {
        "Lanes"
    }

    fn availability(&self) -> Availability {
        Availability::Implemented {
            required: &[Capability::Queries, Capability::Commands],
        }
    }

    fn reset(&mut self) {
        self.lanes.clear();
        self.next_cursor = None;
        self.detail = None;
        self.lead_root = None;
        self.lead_root_next = None;
        self.detached.clear();
        self.detached_next = None;
        self.detached_pages = 0;
        self.detached_target_pages = 0;
        self.list_stale = self.thread.is_some();
        self.detached_stale = self.thread.is_some();
        self.root_stale = self.thread.is_some();
        self.detail_stale = self.opened.is_some();
        // What the dialog showed came from the core: it is read again, and nothing is sent
        // from the old reading. A close that was sent keeps its tag.
        if let Some(dialog) = &mut self.close {
            dialog.phase = close::ClosePhase::Loading;
            dialog.read_due = true;
        }
    }

    fn event(&mut self, event: &UiEvent) {
        let UiEvent::Lanes(LanesEvent::LaneChanged { lane, also_for }) = event else {
            return;
        };
        // A lane another client moved to a lead that is not ours has no tie to us in the
        // event except `also_for`, or the row we already show for it.
        let ours = self.thread.is_some()
            && (lane.lead_thread_id == self.thread
                || self.thread.as_ref().is_some_and(|t| also_for.contains(t))
                || self.lanes.iter().any(|l| l.lane_id == lane.lane_id)
                || self.detached.iter().any(|l| l.lane_id == lane.lane_id));
        if ours {
            self.list_stale = true;
        }
        // A detached lane has no lead: it is told apart only by asking the core again.
        if self.thread.is_some() && (ours || lane.lead_thread_id.is_none()) {
            self.detached_stale = true;
        }
        if self.opened.as_ref() == Some(&lane.lane_id) {
            self.detail_stale = true;
        }
        if let Some(dialog) = &mut self.close
            && dialog.lane == lane.lane_id
            && dialog.moved_by(lane)
        {
            dialog.stale = true;
        }
    }

    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        let Some(pending) = self.pending.remove(&tag) else {
            return;
        };
        match (pending, reply) {
            (Pending::Command(label), FeatureReply::Command(Ok(_))) => {
                self.status = format!("{label}: 受け付けました");
                self.list_stale = true;
                self.detached_stale = true;
                self.detail_stale = self.opened.is_some();
                self.root_stale = true;
            }
            (Pending::Command(label), FeatureReply::Command(Err(error))) => {
                self.status = failure(label, &error);
                if error.outcome_unknown {
                    self.pending.insert(tag, Pending::Command(label));
                }
                self.list_stale = true;
                self.detached_stale = true;
                self.detail_stale = self.opened.is_some();
                self.root_stale = true;
            }
            (Pending::CloseCommand, FeatureReply::Command(Ok(reply))) => {
                let closed = match reply.result.as_deref() {
                    Some(CommandResult::Lanes(LanesResult::Closed(result))) => Some(result.clone()),
                    _ => None,
                };
                self.status = match closed {
                    Some(result) if result.already_closed => format!(
                        "lane の close: 既に終了済みでした（{}）。新しい記録は作っていません",
                        close::close_label(result.kind)
                    ),
                    Some(result) => format!(
                        "lane の close: {} として記録しました。run の停止や削除は行っていません",
                        close::close_label(result.kind)
                    ),
                    None => "lane の close: 受け付けました".to_owned(),
                };
                self.close = None;
                self.refresh_after_command();
            }
            (Pending::CloseCommand, FeatureReply::Command(Err(error))) => {
                if error.outcome_unknown {
                    // Not a success: the same tag answers again with what the core resolved.
                    self.pending.insert(tag, Pending::CloseCommand);
                    self.status = failure("lane の close", &error);
                    if let Some(dialog) = &mut self.close {
                        dialog.notice = Some(
                            "結果が分かりません。成功とは見なさず、台帳を読み直しています。"
                                .to_owned(),
                        );
                        dialog.phase = close::ClosePhase::Loading;
                        dialog.read_due = true;
                    }
                } else if let Some(dialog) = &mut self.close {
                    let stale = error.code == Some(ErrorCode::StaleRevision)
                        || error.message.starts_with("stale");
                    dialog.stale |= stale;
                    dialog.notice = Some(if stale {
                        format!(
                            "lane が変わっていたため記録していません（{}）。Reload すると今の状態が確認対象になります。",
                            error.message
                        )
                    } else {
                        format!("記録していません: {}", error.message)
                    });
                } else {
                    self.status = failure("lane の close", &error);
                }
                self.refresh_after_command();
            }
            (
                Pending::ClosePreview { lane, seq },
                FeatureReply::Query(Ok(QueryReply::Lanes(LanesReply::LaneClosePreview(preview)))),
            ) => {
                if let Some(dialog) = &mut self.close
                    && dialog.lane == lane
                    && dialog.read_seq == seq
                {
                    dialog.phase = close::ClosePhase::Loaded(preview);
                    dialog.stale = false;
                }
            }
            (Pending::ClosePreview { lane, seq }, FeatureReply::Query(Err(error))) => {
                if let Some(dialog) = &mut self.close
                    && dialog.lane == lane
                    && dialog.read_seq == seq
                {
                    dialog.phase = close::ClosePhase::Unavailable(error.message);
                }
            }
            (_, FeatureReply::Query(Err(error))) => self.status = error.message,
            (
                Pending::List { thread, append },
                FeatureReply::Query(Ok(QueryReply::Lanes(LanesReply::Lanes(page)))),
            ) => {
                if self.thread.as_ref() != Some(&thread) {
                    return;
                }
                if append {
                    for lane in page.items {
                        if !self.lanes.iter().any(|l| l.lane_id == lane.lane_id) {
                            self.lanes.push(lane);
                        }
                    }
                } else {
                    self.lanes = page.items;
                }
                self.next_cursor = page.next_cursor;
            }
            (
                Pending::Detached { thread, append },
                FeatureReply::Query(Ok(QueryReply::Lanes(LanesReply::Lanes(page)))),
            ) => {
                if self.thread.as_ref() != Some(&thread) {
                    return;
                }
                if append {
                    for lane in page.items {
                        if !self.detached.iter().any(|l| l.lane_id == lane.lane_id) {
                            self.detached.push(lane);
                        }
                    }
                    self.detached_pages += 1;
                } else {
                    self.detached = page.items;
                    self.detached_pages = 1;
                }
                self.detached_next = page.next_cursor;
                // Pages the user had loaded before a refresh are read again, one at a time.
                if self.detached_pages < self.detached_target_pages && self.detached_next.is_some()
                {
                    self.detached_more_requested = true;
                }
            }
            (
                Pending::Detail { lane },
                FeatureReply::Query(Ok(QueryReply::Lanes(LanesReply::Lane(detail)))),
            ) => {
                if self.opened.as_ref() == Some(&lane) {
                    self.detail = Some(*detail);
                }
            }
            (
                Pending::Management { lane },
                FeatureReply::Query(Ok(QueryReply::Lanes(LanesReply::LaneManagement(state)))),
            ) => {
                if let Some(flow) = &mut self.reattach
                    && flow.lane == lane
                {
                    flow.state = Some(state);
                }
            }
            (
                Pending::LeadRoot { thread },
                FeatureReply::Query(Ok(QueryReply::Execution(ExecutionReply::RunDetails(page)))),
            ) => {
                if self.thread.as_ref() != Some(&thread) {
                    return;
                }
                if let Some(last) = page.items.last() {
                    self.lead_root = Some(last.summary.id.clone());
                }
                self.lead_root_next = page.next_cursor;
            }
            _ => {}
        }
    }

    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        let selected = ctx.selection().thread.clone();
        if selected != self.thread {
            self.switch_thread(selected);
        }
        let Some(thread) = self.thread.clone() else {
            ui.label("lead の会話を選択すると、その lane を表示します。");
            return;
        };
        if self.list_stale || self.more_requested {
            let append = self.more_requested && !self.list_stale;
            self.list_stale = false;
            self.more_requested = false;
            let tag = self.tag(Pending::List {
                thread: thread.clone(),
                append,
            });
            ctx.query(
                tag,
                Query::Lanes(LanesQuery::ListLanes(ListLanes {
                    lead_thread: thread.clone(),
                    policy: None,
                    detached: false,
                    page: PageRequest {
                        cursor: if append {
                            self.next_cursor.clone()
                        } else {
                            None
                        },
                        ..PageRequest::default()
                    },
                })),
            );
        }
        if self.detached_stale || self.detached_more_requested {
            let append = self.detached_more_requested && !self.detached_stale;
            if !append {
                // A refresh answers a request for more as well: it reads one page past the
                // ones shown, and the next pages follow its reply.
                self.detached_target_pages = self
                    .detached_target_pages
                    .max(self.detached_pages.max(1) + usize::from(self.detached_more_requested));
            }
            self.detached_stale = false;
            self.detached_more_requested = false;
            let tag = self.tag(Pending::Detached {
                thread: thread.clone(),
                append,
            });
            ctx.query(
                tag,
                Query::Lanes(LanesQuery::ListLanes(ListLanes {
                    lead_thread: thread.clone(),
                    policy: None,
                    detached: true,
                    page: PageRequest {
                        cursor: if append {
                            self.detached_next.clone()
                        } else {
                            None
                        },
                        ..PageRequest::default()
                    },
                })),
            );
        }
        if self.detail_stale {
            self.detail_stale = false;
            if let Some(lane) = self.opened.clone() {
                let tag = self.tag(Pending::Detail { lane: lane.clone() });
                ctx.query(tag, Query::Lanes(LanesQuery::GetLane { lane_id: lane }));
            }
        }
        if self.root_stale || self.lead_root_next.is_some() {
            let cursor = if self.root_stale {
                None
            } else {
                self.lead_root_next.take()
            };
            self.root_stale = false;
            let tag = self.tag(Pending::LeadRoot {
                thread: thread.clone(),
            });
            ctx.query(
                tag,
                Query::Execution(ExecutionQuery::ListRunDetails(RunDetailsQuery {
                    thread: thread.clone(),
                    scope: RunScope::Roots,
                    page: PageRequest {
                        cursor,
                        ..PageRequest::default()
                    },
                })),
            );
        }
        self.drive_reattach(ctx);
        if !self.status.is_empty() {
            ui.label(&self.status);
        }
        if self.lanes.is_empty() {
            ui.label("この会話が管理している lane はありません。");
        }
        // Each list scrolls in its own bounded area, so that however many lanes the core
        // holds, the buttons that page through them stay inside the window.
        if !self.lanes.is_empty() {
            bounded_list(ui, "lanes-managed", |ui| {
                for lane in self.lanes.clone() {
                    self.draw_lane_row(ui, ctx, &lane);
                }
            });
        }
        if self.next_cursor.is_some() && ui.button("さらに読み込む").clicked() {
            self.more_requested = true;
        }
        if !self.detached.is_empty() {
            bounded_list(ui, "lanes-detached", |ui| {
                for detached in self.detached.clone() {
                    self.draw_detached_row(ui, ctx, detached);
                }
            });
        }
        if self.detached_next.is_some() && ui.button("切り離し済みをさらに読み込む").clicked()
        {
            self.detached_more_requested = true;
        }
        self.draw_detail(ui, ctx);
        self.drive_close(ui, ctx);
    }
}

#[cfg(test)]
mod tests;
