//! Goal submission pane.

use crate::model::commands::{GoalFormModel, PacketReference, ReferenceKind};

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
}

pub fn goal_pane(
    ui: &mut egui::Ui,
    goal: &GoalFormModel,
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

    let sync = GoalFormSync::from(&draft);
    ctx.data_mut(|data| data.insert_temp(egui::Id::new(DRAFT_ID), draft));
    if submit {
        Some(GoalAction::Submit)
    } else {
        Some(GoalAction::SyncForm(sync))
    }
}
