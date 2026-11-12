//! The lane attention section of the Notifications panel: what the lane ledger says needs a
//! person, kept apart from the change notices below it (reading or clearing those resolves
//! nothing here).
//!
//! The pane decides nothing. It draws the rows it is given and returns the operation the user
//! chose; the operations are the ones that already exist (open the lane, continue the lead,
//! resume held notices, answer a question the ledger leaves to the user). A question the lead
//! owns gets no answer control, only [`LEAD_HANDLING_LABEL`]. Stall nudges, forwarding and
//! verification are shown, never offered, until their operations are merged. Closing and
//! discarding by hand are offered only where the window can record them (`can_close`), and
//! only open a confirmation: nothing is recorded from the row itself.

use std::collections::BTreeMap;

use egui::{RichText, Ui};
use workbench_protocol::lane_attention::{
    LaneAttentionAction, LaneAttentionKind, LaneNudgeState, LaneReadyState, LaneStallClassification,
};

use crate::model::lane_attention::{
    AttentionAction, AttentionBoard, AttentionOp, AttentionRow, Destination, LEAD_HANDLING_LABEL,
};
use crate::model::lane_close::{CloseChoice, MEANING};
use crate::panes::requests::{ConversationRequests, RequestAction};
use crate::theme::text::muted;
use crate::theme::tokens::{R_SM, STATUS_STROKE, attention_color, palette};

/// What the user chose in the section.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AttentionPaneOutput {
    pub action: Option<AttentionAction>,
    /// An answer typed into an inline question card, run by the usual answer path.
    pub request: Option<RequestAction>,
}

/// The one-line name of why an item is listed.
#[must_use]
pub fn kind_label(kind: &LaneAttentionKind) -> &'static str {
    match kind {
        LaneAttentionKind::QuestionPending { .. } => "Question",
        LaneAttentionKind::Blocked { .. } => "Blocked",
        LaneAttentionKind::Failed { .. } => "Failed",
        LaneAttentionKind::RestoreFailed { .. } => "Restore failed",
        LaneAttentionKind::ReadyUnclosed { .. } => "Ready (awaiting close)",
        LaneAttentionKind::QuestionCapacityBlocked { .. } => "Question limit",
        LaneAttentionKind::WakeSuppressed { .. } => "Wake paused",
        LaneAttentionKind::StopPending { .. } => "Notices held",
        LaneAttentionKind::Stall { .. } => "Stalled",
    }
}

/// What the item says beyond its summary, in one sentence; `None` when the summary is enough.
#[must_use]
pub fn kind_detail(kind: &LaneAttentionKind) -> Option<String> {
    Some(match kind {
        LaneAttentionKind::ReadyUnclosed { readiness, .. } => format!(
            "The lane reported ready and waits for its lead to close it ({}). The lead decides; this is not a stall.",
            match readiness {
                LaneReadyState::NoEvidence => "no fixed artifact and no review",
                LaneReadyState::Unreviewed => "not reviewed",
                LaneReadyState::ReviewInProgress => "review under way",
                LaneReadyState::ReviewRejected => "review rejected",
                LaneReadyState::ReviewIncomplete => "review did not finish",
                LaneReadyState::AwaitingClose => "review approved",
            }
        ),
        LaneAttentionKind::QuestionCapacityBlocked { limit, .. } => format!(
            "New questions are refused at {limit} unanswered. Answer some to continue; nothing is retried automatically."
        ),
        LaneAttentionKind::WakeSuppressed { .. } => {
            "Wakes of the lead were paused after wakes that brought no progress. Its notices are kept."
                .to_owned()
        }
        LaneAttentionKind::StopPending { pending_count, .. } => format!(
            "The delivery target was stopped: {pending_count} notice(s) are held, not lost, and it is not woken until continued."
        ),
        LaneAttentionKind::Stall {
            classification,
            nudge,
            ..
        } => format!(
            "{} {}",
            match classification {
                LaneStallClassification::NoReport => "The lane has not reported for a long time.",
                LaneStallClassification::WorkStalled => "The lane's work has not advanced.",
                LaneStallClassification::WaitingQuestion => "The lane waits for an answer.",
                LaneStallClassification::SilentDone => "The lane finished without reporting.",
                LaneStallClassification::RestoreUnavailable => "The lane cannot be resumed.",
            },
            match nudge {
                LaneNudgeState::NotRequested => "No reminder was sent.",
                LaneNudgeState::Pending => "A reminder is waiting to be delivered.",
                LaneNudgeState::Accepted => "A reminder was delivered.",
                LaneNudgeState::Suppressed => "A reminder was held back.",
                LaneNudgeState::Obsolete => "The reminder is no longer needed.",
                LaneNudgeState::Failed => "The reminder could not be delivered.",
            }
        ),
        LaneAttentionKind::RestoreFailed { attempts, suppressed, .. } => {
            format!("Recovery failed {attempts} time(s). {}", if *suppressed {
                "Automatic retries are suppressed; explicitly Resume after checking the recovery source."
            } else { "Held notices remain pending." })
        }
        LaneAttentionKind::QuestionPending { .. }
        | LaneAttentionKind::Blocked { .. }
        | LaneAttentionKind::Failed { .. } => return None,
    })
}

pub fn lane_attention_section(
    ui: &mut Ui,
    board: &AttentionBoard,
    rows: &[AttentionRow<'_>],
    drafts: &mut BTreeMap<String, String>,
    can_close: bool,
) -> AttentionPaneOutput {
    let mut output = AttentionPaneOutput::default();
    if !board.has_reads() {
        return output;
    }
    egui::CollapsingHeader::new(format!("Needs attention ({})", rows.len()))
        .id_salt("lane-attention")
        .default_open(true)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(muted(
                    "Read from the lane ledger. Reading or clearing notifications does not resolve these.",
                ));
                if ui.small_button("Refresh").clicked() {
                    output.action = Some(AttentionAction {
                        lead: String::new(),
                        item: None,
                        op: AttentionOp::Refresh,
                    });
                }
            });
            for (lead, error) in board.errors() {
                ui.colored_label(
                    palette().WARNING_FG,
                    format!("Could not read the list for {lead}: {error}"),
                );
            }
            if board.truncated() {
                ui.label(muted("Only the newest items are listed; resolve some to see more."));
            }
            if rows.is_empty() {
                ui.label(muted("No lane needs attention."));
            }
            egui::ScrollArea::vertical()
                .id_salt("lane-attention-rows")
                .max_height(320.0)
                .show(ui, |ui| {
                    for row in rows {
                        ui.push_id(row.item.id.as_str(), |ui| row_card(ui, row, drafts, can_close, &mut output));
                    }
                });
        });
    ui.separator();
    output
}

fn row_card(
    ui: &mut Ui,
    row: &AttentionRow<'_>,
    drafts: &mut BTreeMap<String, String>,
    can_close: bool,
    output: &mut AttentionPaneOutput,
) {
    let item = row.item;
    egui::Frame::new()
        .stroke(egui::Stroke::new(STATUS_STROKE, attention_color()))
        .corner_radius(egui::CornerRadius::same(R_SM))
        .inner_margin(egui::Margin::same(4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(kind_label(&item.kind))
                        .strong()
                        .color(attention_color()),
                );
                ui.label(RichText::new(&row.lane_title).strong());
                if row.lead_title != row.lane_title {
                    ui.label(muted(format!("lead: {}", row.lead_title)));
                }
            });
            if !item.summary.is_empty() {
                ui.add(egui::Label::new(&item.summary).wrap());
            }
            if let Some(detail) = kind_detail(&item.kind) {
                ui.add(egui::Label::new(muted(detail)).wrap());
            }
            let chosen = |op| AttentionAction {
                lead: item.lead_thread.to_string(),
                item: Some((item.id.to_string(), item.revision.get())),
                op,
            };
            let allows = |action| item.available_actions.contains(&action);
            let mut op = None;
            match &item.kind {
                LaneAttentionKind::QuestionPending { .. }
                    if crate::model::lane_attention::lead_handles(&item.kind) =>
                {
                    ui.label(muted(LEAD_HANDLING_LABEL));
                }
                LaneAttentionKind::QuestionPending { .. } => {
                    if let Some(question) = row.answerable
                        && let Some(thread) = &row.lane_thread
                    {
                        let thread = thread.to_string();
                        let mut requests = ConversationRequests {
                            thread_id: &thread,
                            approvals: Vec::new(),
                            questions: vec![question],
                            drafts,
                        };
                        if let Some(request) = requests.show(ui) {
                            output.request = Some(request);
                        }
                    }
                }
                _ => {}
            }
            ui.horizontal_wrapped(|ui| {
                if let Some(thread) = &row.lane_thread
                    && allows(LaneAttentionAction::Inspect)
                    && ui.small_button("Open lane").clicked()
                {
                    op = Some(AttentionOp::OpenLane(thread.clone()));
                }
                // (button label, hover text, what it does): only operations that exist today.
                let lead = item.lead_thread.to_string();
                let held_thread = |label_lead: &'static str| match &row.destination {
                    Destination::Lead => Some((
                        label_lead,
                        "Resume the lead's conversation; held notices are then delivered",
                        AttentionOp::ContinueThread(lead.clone()),
                    )),
                    Destination::Lane(thread) => Some((
                        "Continue lane",
                        "Resume this lane's conversation; held notices are then delivered",
                        AttentionOp::ContinueThread(thread.to_string()),
                    )),
                    Destination::Unknown => None,
                };
                let offered = match &item.kind {
                    LaneAttentionKind::Blocked { .. }
                    | LaneAttentionKind::Failed { .. }
                    | LaneAttentionKind::Stall { .. }
                        if allows(LaneAttentionAction::Continue) =>
                    {
                        Some((
                            "Continue lead",
                            "Resume the lead's conversation",
                            AttentionOp::ContinueThread(lead.clone()),
                        ))
                    }
                    LaneAttentionKind::RestoreFailed {
                        suppressed: true, ..
                    } if allows(LaneAttentionAction::Resume) => match &row.destination {
                        Destination::Lead => Some((
                            "Resume lead",
                            "Explicitly retry held notices for this recovery destination",
                            AttentionOp::ResumeThread(lead.clone()),
                        )),
                        Destination::Lane(thread) => Some((
                            "Resume lane",
                            "Explicitly retry held notices for this recovery destination",
                            AttentionOp::ResumeThread(thread.to_string()),
                        )),
                        Destination::Unknown => None,
                    },
                    LaneAttentionKind::RestoreFailed {
                        suppressed: false, ..
                    } if allows(LaneAttentionAction::Continue) => held_thread("Continue lead"),
                    LaneAttentionKind::StopPending { .. }
                        if allows(LaneAttentionAction::Resume) =>
                    {
                        held_thread("Continue lead")
                    }
                    LaneAttentionKind::WakeSuppressed { .. }
                        if allows(LaneAttentionAction::Resume) =>
                    {
                        Some((
                            "Resume notifications",
                            "Deliver this lane's held notices to its lead again",
                            AttentionOp::ResumeDelivery,
                        ))
                    }
                    _ => None,
                };
                if let Some((label, hover, offered)) = offered
                    && ui.small_button(label).on_hover_text(hover).clicked()
                {
                    op = Some(offered);
                }
                if can_close {
                    let ready = matches!(&item.kind, LaneAttentionKind::ReadyUnclosed { .. });
                    if ready
                        && allows(LaneAttentionAction::Close)
                        && ui
                            .small_button(CloseChoice::Complete.button())
                            .on_hover_text(MEANING)
                            .clicked()
                    {
                        op = Some(AttentionOp::BeginClose(CloseChoice::Complete));
                    }
                    let endable = ready
                        || matches!(
                            &item.kind,
                            LaneAttentionKind::Blocked { .. }
                                | LaneAttentionKind::Failed { .. }
                                | LaneAttentionKind::Stall { .. }
                        );
                    if endable
                        && ui
                            .small_button(CloseChoice::Discard.button())
                            .on_hover_text(MEANING)
                            .clicked()
                    {
                        op = Some(AttentionOp::BeginClose(CloseChoice::Discard));
                    }
                }
            });
            if let Some(op) = op {
                output.action = Some(chosen(op));
            }
        });
    ui.add_space(2.0);
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use workbench_protocol::ids::{QuestionId, ThreadId as WireThread};
    use workbench_protocol::lane_attention::{
        LaneAttentionItem, LaneAttentionItemId, LaneAttentionSnapshot, LaneId,
        LaneQuestionResponder,
    };
    use workbench_protocol::wire::{Revision, Sequence, UnixMillis};
    use workspace_ui::ThreadId;

    use super::*;
    use crate::model::lane_attention::LeadRead;

    fn item(
        id: &str,
        kind: LaneAttentionKind,
        actions: &[LaneAttentionAction],
    ) -> LaneAttentionItem {
        LaneAttentionItem {
            id: LaneAttentionItemId::new(id).unwrap(),
            lane: LaneId::new("lane-1").unwrap(),
            lead_thread: WireThread::new("lead").unwrap(),
            management_revision: Revision(1),
            revision: Revision(7),
            kind,
            since: UnixMillis(1),
            summary: format!("summary of {id}"),
            evidence_refs: vec![],
            available_actions: actions.to_vec(),
        }
    }

    fn question_kind(responder: LaneQuestionResponder) -> LaneAttentionKind {
        LaneAttentionKind::QuestionPending {
            question: QuestionId::new("q").unwrap(),
            responder,
            routing_revision: Revision(1),
        }
    }

    fn user_question(responder: event_bus::lane::QuestionResponder) -> event_bus::UserQuestion {
        event_bus::UserQuestion {
            responder: Some(responder),
            routing_revision: 1,
            management_revision: Some(1),
            id: "q".into(),
            run_id: "lane-root".into(),
            root_run_id: "lane-root".into(),
            root_name: "chat:Orchestrator:lane-thread".into(),
            recipient_run_ids: vec![],
            title: "Which scope?".into(),
            options: vec![],
            blocking: false,
            answer: None,
            answer_origin: None,
        }
    }

    type Captured = Rc<RefCell<AttentionPaneOutput>>;

    /// Draws the section over `items` (the lane's thread is known), collecting what is chosen.
    fn draw(
        items: Vec<LaneAttentionItem>,
        question: Option<event_bus::UserQuestion>,
    ) -> (Harness<'static>, Captured) {
        draw_with(items, question, false)
    }

    fn draw_with(
        items: Vec<LaneAttentionItem>,
        question: Option<event_bus::UserQuestion>,
        can_close: bool,
    ) -> (Harness<'static>, Captured) {
        let captured: Captured = Rc::default();
        let sink = captured.clone();
        let mut board = AttentionBoard::default();
        board.apply(
            "lead",
            1,
            Ok(LeadRead {
                items,
                snapshot: LaneAttentionSnapshot::new("s").unwrap(),
                truncated: false,
                questions: question.into_iter().map(|q| (q.id.clone(), q)).collect(),
            }),
        );
        let mut threads = vec![workspace_ui::ThreadRecord::new(
            ThreadId::new("lane-thread"),
            workspace_ui::ProjectId::new("p"),
            "Lane one",
        )];
        threads[0].lane = Some(workspace_ui::LaneLink {
            detached_since_management_revision: None,
            lane_id: "lane-1".into(),
            origin_thread_id: ThreadId::new("lead"),
            lead_thread_id: Some(ThreadId::new("lead")),
            policy: workspace_ui::LanePolicy::Attached,
            management_revision: 1,
        });
        let mut drafts = BTreeMap::from([("q".to_owned(), "narrow".to_owned())]);
        let harness = Harness::builder()
            .with_size(egui::vec2(640.0, 600.0))
            .build_ui(move |ui| {
                crate::theme::install(ui.ctx());
                let rows = crate::model::lane_attention::rows(&board, &threads);
                let output = lane_attention_section(ui, &board, &rows, &mut drafts, can_close);
                if output.action.is_some() || output.request.is_some() {
                    *sink.borrow_mut() = output;
                }
            });
        (harness, captured)
    }

    #[test]
    fn a_question_the_lead_owns_has_no_answer_control_only_the_lead_is_handling_it() {
        let lead = question_kind(LaneQuestionResponder::Lead {
            lead_thread: WireThread::new("lead").unwrap(),
        });
        let (mut harness, captured) = draw(
            vec![item(
                "q",
                lead,
                &[
                    LaneAttentionAction::Answer,
                    LaneAttentionAction::RouteToUser,
                ],
            )],
            // Even a projection that (wrongly) says the user may see it draws no control.
            Some(user_question(event_bus::lane::QuestionResponder::User)),
        );
        harness.run_steps(2);

        assert!(harness.query_by_label(LEAD_HANDLING_LABEL).is_some());
        assert!(harness.query_by_label("Send answer").is_none());
        assert!(
            harness.query_by_label("Open lane").is_none(),
            "Inspect is not allowed here"
        );
        assert_eq!(*captured.borrow(), AttentionPaneOutput::default());
    }

    #[test]
    fn once_routed_to_the_user_the_question_is_answered_in_the_list_by_the_usual_path() {
        let (mut harness, captured) = draw(
            vec![item(
                "q",
                question_kind(LaneQuestionResponder::User),
                &[LaneAttentionAction::Inspect],
            )],
            Some(user_question(event_bus::lane::QuestionResponder::User)),
        );
        harness.run_steps(2);
        assert!(harness.query_by_label(LEAD_HANDLING_LABEL).is_none());

        harness.get_by_label("Send answer").click();
        harness.run_steps(2);

        assert_eq!(
            captured.borrow().request,
            Some(RequestAction::Answer {
                thread_id: "lane-thread".into(),
                question_id: "q".into(),
                answer: "narrow".into(),
            })
        );
    }

    #[test]
    fn a_blocked_lane_offers_the_existing_open_and_continue_operations_on_that_item() {
        let (mut harness, captured) = draw(
            vec![item(
                "b",
                LaneAttentionKind::Blocked {
                    report_seq: Sequence(2),
                },
                &[LaneAttentionAction::Inspect, LaneAttentionAction::Continue],
            )],
            None,
        );
        harness.run_steps(2);
        harness.get_by_label("Open lane").click();
        harness.run_steps(2);
        let action = captured.borrow_mut().action.take().unwrap();
        assert_eq!(action.lead, "lead");
        assert_eq!(action.item, Some(("b".into(), 7)));
        assert_eq!(
            action.op,
            AttentionOp::OpenLane(ThreadId::new("lane-thread"))
        );

        harness.get_by_label("Continue lead").click();
        harness.run_steps(2);
        assert_eq!(
            captured.borrow().action.as_ref().unwrap().op,
            AttentionOp::ContinueThread("lead".into())
        );
    }

    #[test]
    fn a_paused_wake_resumes_notifications_and_a_stop_continues_the_lead() {
        let (mut harness, captured) = draw(
            vec![item(
                "w",
                LaneAttentionKind::WakeSuppressed {
                    guard_revision: Revision(3),
                },
                &[LaneAttentionAction::Resume],
            )],
            None,
        );
        harness.run_steps(2);
        harness.get_by_label("Resume notifications").click();
        harness.run_steps(2);
        assert_eq!(
            captured.borrow().action.as_ref().unwrap().op,
            AttentionOp::ResumeDelivery
        );

        let (mut harness, captured) = draw(
            vec![item(
                "s",
                LaneAttentionKind::StopPending {
                    destination_thread: WireThread::new("lead").unwrap(),
                    pending_count: 2,
                },
                &[LaneAttentionAction::Resume],
            )],
            None,
        );
        harness.run_steps(2);
        assert!(harness.query_by_label("Resume notifications").is_none());
        harness.get_by_label("Continue lead").click();
        harness.run_steps(2);
        assert_eq!(
            captured.borrow().action.as_ref().unwrap().op,
            AttentionOp::ContinueThread("lead".into())
        );
    }

    #[test]
    fn verification_is_shown_but_not_offered_and_close_needs_a_window_that_can_record_it() {
        let (mut harness, captured) = draw(
            vec![item(
                "r",
                LaneAttentionKind::ReadyUnclosed {
                    ready_seq: Sequence(4),
                    readiness: LaneReadyState::Unreviewed,
                },
                &[
                    LaneAttentionAction::Verify,
                    LaneAttentionAction::Close,
                    LaneAttentionAction::Inspect,
                ],
            )],
            None,
        );
        harness.run_steps(2);

        assert!(harness.query_by_label("Ready (awaiting close)").is_some());
        assert!(harness.query_by_label("Open lane").is_some());
        for label in ["Verify", "Verify lane", "Close…", "Discard…"] {
            assert!(harness.query_by_label(label).is_none(), "{label}");
        }
        assert_eq!(*captured.borrow(), AttentionPaneOutput::default());
    }

    #[test]
    fn a_ready_lane_offers_close_and_discard_that_only_open_the_confirmation() {
        let ready = || {
            item(
                "r",
                LaneAttentionKind::ReadyUnclosed {
                    ready_seq: Sequence(4),
                    readiness: LaneReadyState::Unreviewed,
                },
                &[LaneAttentionAction::Close, LaneAttentionAction::Inspect],
            )
        };
        let (mut harness, captured) = draw_with(vec![ready()], None, true);
        harness.run_steps(2);
        harness.get_by_label("Close…").click();
        harness.run_steps(2);
        let action = captured.borrow_mut().action.take().unwrap();
        assert_eq!(action.item, Some(("r".into(), 7)));
        assert_eq!(action.op, AttentionOp::BeginClose(CloseChoice::Complete));

        harness.get_by_label("Discard…").click();
        harness.run_steps(2);
        assert_eq!(
            captured.borrow().action.as_ref().unwrap().op,
            AttentionOp::BeginClose(CloseChoice::Discard)
        );
    }

    #[test]
    fn a_blocked_lane_can_be_discarded_but_not_closed_as_done() {
        let blocked = item(
            "b",
            LaneAttentionKind::Blocked {
                report_seq: Sequence(2),
            },
            &[LaneAttentionAction::Inspect, LaneAttentionAction::Continue],
        );
        let (mut harness, _captured) = draw_with(vec![blocked], None, true);
        harness.run_steps(2);
        assert!(harness.query_by_label("Close…").is_none());
        assert!(harness.query_by_label("Discard…").is_some());
    }

    #[test]
    fn a_question_row_has_no_way_to_end_the_lane() {
        let (mut harness, _captured) = draw_with(
            vec![item(
                "q",
                question_kind(LaneQuestionResponder::User),
                &[LaneAttentionAction::Inspect],
            )],
            Some(user_question(event_bus::lane::QuestionResponder::User)),
            true,
        );
        harness.run_steps(2);
        assert!(harness.query_by_label("Close…").is_none());
        assert!(harness.query_by_label("Discard…").is_none());
    }

    #[test]
    fn held_notices_are_continued_on_the_thread_whose_delivery_is_held() {
        let stop = |destination: &str| {
            item(
                "s",
                LaneAttentionKind::StopPending {
                    destination_thread: WireThread::new(destination).unwrap(),
                    pending_count: 1,
                },
                &[LaneAttentionAction::Resume],
            )
        };

        // The lead was stopped: the lead is continued.
        let (mut harness, captured) = draw(vec![stop("lead")], None);
        harness.run_steps(2);
        assert!(harness.query_by_label("Continue lane").is_none());
        harness.get_by_label("Continue lead").click();
        harness.run_steps(2);
        assert_eq!(
            captured.borrow().action.as_ref().unwrap().op,
            AttentionOp::ContinueThread("lead".into())
        );

        // The lane was stopped: the lane is continued, never the lead.
        let (mut harness, captured) = draw(vec![stop("lane-thread")], None);
        harness.run_steps(2);
        assert!(harness.query_by_label("Continue lead").is_none());
        harness.get_by_label("Continue lane").click();
        harness.run_steps(2);
        assert_eq!(
            captured.borrow().action.as_ref().unwrap().op,
            AttentionOp::ContinueThread("lane-thread".into())
        );

        // A thread the window does not show as a lane or the lead gets no operation.
        let (mut harness, captured) = draw(vec![stop("somewhere-else")], None);
        harness.run_steps(2);
        assert!(harness.query_by_label("Continue lead").is_none());
        assert!(harness.query_by_label("Continue lane").is_none());
        assert_eq!(*captured.borrow(), AttentionPaneOutput::default());
    }

    #[test]
    fn a_suppressed_recovery_offers_explicit_resume_of_its_own_destination() {
        let failed = item(
            "r",
            LaneAttentionKind::RestoreFailed {
                destination_thread: WireThread::new("lane-thread").unwrap(),
                failure_ref: "lane_delivery:lane-thread".into(),
                attempts: 3,
                suppressed: true,
            },
            &[LaneAttentionAction::Inspect, LaneAttentionAction::Resume],
        );
        let (mut harness, captured) = draw(vec![failed], None);
        harness.run_steps(2);
        assert!(harness.query_by_label("Resume lead").is_none());
        harness.get_by_label("Resume lane").click();
        harness.run_steps(2);
        assert_eq!(
            captured.borrow().action.as_ref().unwrap().op,
            AttentionOp::ResumeThread("lane-thread".into())
        );
    }

    #[test]
    fn nothing_is_drawn_before_any_list_was_read() {
        let captured: Captured = Rc::default();
        let board = AttentionBoard::default();
        let mut drafts = BTreeMap::new();
        let mut harness = Harness::builder().build_ui(move |ui| {
            let output = lane_attention_section(ui, &board, &[], &mut drafts, true);
            *captured.borrow_mut() = output;
        });
        harness.run_steps(2);
        assert!(harness.query_by_label("Refresh").is_none());
    }
}
