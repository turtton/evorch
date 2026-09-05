//! Goal submission pane.

use event_bus::{CloseoutStep, GoalStage, GoalState};

use crate::model::commands::{GoalFormModel, LoopStatusView, PacketReference, ReferenceKind};

const DRAFT_ID: &str = "goal-pane-draft";

/// Goal フォームの編集中ドラフトです。`GoalFormModel` のうち編集可能な 3 フィールドを保持します。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoalDraft {
    /// goal 本文です。
    pub goal: String,
    /// 参照行です。
    pub references: Vec<PacketReference>,
    /// 制約行です。
    pub constraints: Vec<String>,
}

impl GoalDraft {
    fn from_model(model: &GoalFormModel) -> Self {
        Self {
            goal: model.goal.clone(),
            references: model.references.clone(),
            constraints: model.constraints.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalFormSync {
    pub goal: String,
    pub references: Vec<PacketReference>,
    pub constraints: Vec<String>,
}

impl From<&GoalDraft> for GoalFormSync {
    fn from(draft: &GoalDraft) -> Self {
        Self {
            goal: draft.goal.clone(),
            references: draft.references.clone(),
            constraints: draft.constraints.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalAction {
    Submit,
    SyncForm(GoalFormSync),
    PauseGoal,
    ResumeGoal,
    CancelGoal,
}

pub fn goal_pane(
    ui: &mut egui::Ui,
    goal: &GoalFormModel,
    status: &LoopStatusView,
    blocked: Option<&str>,
    has_active_thread: bool,
) -> Option<GoalAction> {
    let ctx = ui.ctx().clone();
    let stored = ctx
        .data_mut(|data| data.get_temp::<GoalDraft>(egui::Id::new(DRAFT_ID)))
        .unwrap_or_else(|| GoalDraft::from_model(goal));
    let mut draft = if stored.goal == goal.goal
        && stored.references == goal.references
        && stored.constraints == goal.constraints
    {
        stored
    } else {
        GoalDraft::from_model(goal)
    };
    let mut action = None;

    ui.heading("Goal");
    ui.label("Goal text");
    ui.add(
        egui::TextEdit::multiline(&mut draft.goal)
            .desired_rows(4)
            .desired_width(f32::INFINITY),
    );

    ui.label("References");
    for index in 0..draft.references.len() {
        ui.horizontal(|ui| {
            let reference = &mut draft.references[index];
            let kind_label = match reference.kind {
                ReferenceKind::Packet => "packet",
                ReferenceKind::Issue => "issue",
            };
            if ui.button(kind_label).clicked() {
                reference.kind = match reference.kind {
                    ReferenceKind::Packet => ReferenceKind::Issue,
                    ReferenceKind::Issue => ReferenceKind::Packet,
                };
            }
            ui.add(
                egui::TextEdit::singleline(&mut reference.value)
                    .hint_text("reference id")
                    .desired_width(160.0),
            );
        });
    }
    if ui.button("Add reference").clicked() {
        draft.references.push(PacketReference {
            kind: ReferenceKind::Packet,
            value: String::new(),
        });
    }

    ui.label("Constraints");
    for constraint in &mut draft.constraints {
        ui.add(egui::TextEdit::singleline(constraint).desired_width(f32::INFINITY));
    }
    if ui.button("Add constraint").clicked() {
        draft.constraints.push(String::new());
    }

    if !has_active_thread {
        ui.label("no active thread");
    }
    let submit = ui
        .add_enabled(has_active_thread, egui::Button::new("Submit"))
        .clicked();

    if let Some(goal_id) = &goal.last_accepted {
        ui.label(format!("accepted: {goal_id}"));
    }

    if let Some(state) = status.state {
        ui.label(format!("state: {}", state_label(state)));
    }
    if let Some(stage) = status.stage {
        ui.label(format!("stage: {}", stage_label(stage)));
    }
    ui.label(format!("round: {}", status.review_round));
    ui.label(format!("epoch: {}", status.epoch));
    if status.nudges > 0 {
        ui.label(format!("nudges: {}", status.nudges));
    }
    if let Some(reason) = blocked {
        ui.label(format!("blocked: {reason}"));
    }
    for rejection in &status.last_rejections {
        ui.label(format!("rejected: {rejection}"));
    }
    for (step, ok) in &status.closeout {
        ui.label(format!(
            "closeout: {} {}",
            step_label(*step),
            if *ok { "ok" } else { "failed" }
        ));
    }

    let goal_bound = status.goal_id.is_some();
    let state = status.state;
    let pause_clicked = ui
        .add_enabled(
            goal_bound && state == Some(GoalState::Active),
            egui::Button::new("Pause goal"),
        )
        .clicked();
    let resume_clicked = ui
        .add_enabled(
            goal_bound && matches!(state, Some(GoalState::Paused) | Some(GoalState::Blocked)),
            egui::Button::new("Resume goal"),
        )
        .clicked();
    let cancel_clicked = ui
        .add_enabled(
            goal_bound
                && matches!(
                    state,
                    Some(GoalState::Active) | Some(GoalState::Paused) | Some(GoalState::Blocked)
                ),
            egui::Button::new("Cancel goal"),
        )
        .clicked();
    if pause_clicked {
        action = Some(GoalAction::PauseGoal);
    } else if resume_clicked {
        action = Some(GoalAction::ResumeGoal);
    } else if cancel_clicked {
        action = Some(GoalAction::CancelGoal);
    }

    let sync = GoalFormSync::from(&draft);
    ctx.data_mut(|data| data.insert_temp(egui::Id::new(DRAFT_ID), draft));
    if action.is_some() {
        action
    } else if submit {
        Some(GoalAction::Submit)
    } else {
        Some(GoalAction::SyncForm(sync))
    }
}

fn state_label(state: GoalState) -> &'static str {
    match state {
        GoalState::Active => "active",
        GoalState::Paused => "paused",
        GoalState::Blocked => "blocked",
        GoalState::Complete => "complete",
        GoalState::Cancelled => "cancelled",
    }
}

fn stage_label(stage: GoalStage) -> &'static str {
    match stage {
        GoalStage::Implementing => "implementing",
        GoalStage::Delivering => "delivering",
        GoalStage::AwaitingCi => "awaiting_ci",
        GoalStage::Reviewing => "reviewing",
        GoalStage::Repairing => "repairing",
        GoalStage::ReadyToFinish => "ready_to_finish",
        GoalStage::AwaitingMergeApproval => "awaiting_merge_approval",
        GoalStage::Merging => "merging",
        GoalStage::Closeout => "closeout",
        GoalStage::Done => "done",
    }
}

fn step_label(step: CloseoutStep) -> &'static str {
    match step {
        CloseoutStep::WorkerClaim => "worker_claim",
        CloseoutStep::ResultSummary => "result_summary",
        CloseoutStep::WorkerComplete => "worker_complete",
    }
}
