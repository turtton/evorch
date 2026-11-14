//! Ending a lane by a person's decision: close it as done (unverified) or discard it.
//!
//! The ledger records `human_closed` or `discarded`. Both only end the ledger entry: no run
//! is stopped, no thread or worktree is deleted and nothing is detached. What the person
//! reads comes from the core's close preview, and the revisions that preview named are the
//! ones the close command carries; a lane that moved since is refused by the ledger and
//! nothing is recorded. The dialog never sends a request again by itself.
use workbench_protocol::{
    lane_attention::LaneId,
    lane_management::{CloseLane, HumanLaneCloseKind, LaneCloseRecordKind, MAX_CLOSE_NOTE_BYTES},
    namespaces::lanes::{LaneClosePreview, LaneCurrentClose},
};

/// What closing means, shown in the confirmation.
pub(super) const MEANING: &str = "これは台帳に lane の終了を記録するだけです。実行中の run の停止、スレッドや worktree の削除、切り離しは行いません。人の判断であり、検証ではありません。";

/// What closing leaves as it is.
pub(super) const REMAINS: &str = "未回答の質問は残ります。後から lane が Report を送ると、この close は解除され、lane は再び稼働中として扱われます。";

/// Shown when a run of the lane's thread is live: a close does not stop it.
pub(super) const RUNNING_WARNING: &str = "この lane の run は実行中です。ここで閉じても run は止まりません。止めたい場合は別に停止してください。";

/// Shown when the lane has no Ready for its current management.
pub(super) const NO_READY_WARNING: &str = "この lane は現在の brief に対する Ready を報告していません。読める結果がないまま終了することになります。";

/// Shown when the latest Ready was reported for an earlier brief.
pub(super) const OLDER_BRIEF_WARNING: &str = "最新の Ready は以前の brief に対する報告です。その後 brief が更新されているため参考表示であり、現在の brief に対する結果ではありません。";

pub(super) const NOTE_LABEL: &str = "理由（必須）";

/// Which ending the person chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CloseChoice {
    Complete,
    Discard,
}

impl CloseChoice {
    /// The button that opens the dialog.
    pub(super) const fn button(self) -> &'static str {
        match self {
            Self::Complete => "人間の判断で閉じる…",
            Self::Discard => "破棄する…",
        }
    }

    pub(super) const fn title(self) -> &'static str {
        match self {
            Self::Complete => "lane を閉じる（未検証）",
            Self::Discard => "lane の破棄",
        }
    }

    pub(super) const fn confirm(self) -> &'static str {
        match self {
            Self::Complete => "lane を閉じる",
            Self::Discard => "lane を破棄する",
        }
    }

    pub(super) const fn recorded_as(self) -> &'static str {
        match self {
            Self::Complete => "記録される内容: 人の判断で完了（未検証）",
            Self::Discard => "記録される内容: 破棄",
        }
    }

    pub(super) const fn kind(self) -> HumanLaneCloseKind {
        match self {
            Self::Complete => HumanLaneCloseKind::Complete,
            Self::Discard => HumanLaneCloseKind::Discard,
        }
    }
}

/// The kind of a recorded close in words that never read like a verified one.
pub(super) const fn close_label(kind: LaneCloseRecordKind) -> &'static str {
    match kind {
        LaneCloseRecordKind::LeadClosed => "lead が close",
        LaneCloseRecordKind::HumanClosed => "人の判断で完了（未検証）",
        LaneCloseRecordKind::Discarded => "破棄",
    }
}

/// The badge of a lane whose close stands.
pub(super) fn badge(close: &LaneCurrentClose) -> String {
    format!("close 済み: {}", close_label(close.kind))
}

/// The warning a person closing a lane as done reads about its Ready, if any. A Ready for an
/// earlier brief is for reference only.
pub(super) fn ready_warning(preview: &LaneClosePreview) -> Option<&'static str> {
    match &preview.ready {
        None => Some(NO_READY_WARNING),
        Some(ready) if ready.brief_revision != preview.brief_revision => Some(OLDER_BRIEF_WARNING),
        Some(_) => None,
    }
}

/// How many unanswered questions the lane still holds; `None` when none.
pub(super) fn open_questions_line(count: u32) -> Option<String> {
    (count > 0).then(|| {
        format!("この lane には未回答の質問が {count} 件あります。close しても質問は残ります。")
    })
}

/// What the dialog has read so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ClosePhase {
    Loading,
    Loaded(Box<LaneClosePreview>),
    Unavailable(String),
}

/// The open confirmation.
#[derive(Debug, Clone)]
pub(super) struct CloseDialog {
    pub(super) lane: LaneId,
    pub(super) choice: CloseChoice,
    /// The start of the lane's objective, as the row or detail that opened the dialog showed.
    pub(super) title: String,
    pub(super) phase: ClosePhase,
    pub(super) note: String,
    /// What the last confirmation answered, when it did not record the close.
    pub(super) notice: Option<String>,
    /// The lane moved after the preview was read: read again before deciding.
    pub(super) stale: bool,
    /// Which read the screen waits for; an answer for an older one is dropped.
    pub(super) read_seq: u64,
    /// A read has to be asked for the next time the screen is drawn.
    pub(super) read_due: bool,
}

impl CloseDialog {
    pub(super) fn new(lane: LaneId, choice: CloseChoice, title: String) -> Self {
        Self {
            lane,
            choice,
            title,
            phase: ClosePhase::Loading,
            note: String::new(),
            notice: None,
            stale: false,
            read_seq: 0,
            read_due: true,
        }
    }

    /// Why this screen cannot decide, in a sentence; `None` when it can. Reading and typing
    /// change nothing, so a blocker is only ever a reason not to send.
    pub(super) fn blocker(&self, sending: bool) -> Option<String> {
        if sending {
            return Some("close の結果を待っています。".to_owned());
        }
        if self.stale {
            return Some(
                "この画面を読んだ後に lane が変わりました。Reload してから決めてください。"
                    .to_owned(),
            );
        }
        match &self.phase {
            ClosePhase::Loading => Some("台帳から lane を読み込んでいます…".to_owned()),
            ClosePhase::Unavailable(reason) => Some(reason.clone()),
            ClosePhase::Loaded(preview) => preview.current_close.as_ref().map(|close| {
                format!(
                    "この lane は既に終了が記録されています（{}）。記録するものはありません。",
                    close_label(close.kind)
                )
            }),
        }
    }

    /// The reason is required and bounded by what the ledger accepts.
    pub(super) fn note_error(&self) -> Option<&'static str> {
        let note = self.note.trim();
        if note.is_empty() {
            Some("理由は必須です。")
        } else if note.len() > MAX_CLOSE_NOTE_BYTES {
            Some("理由が長すぎます（2KiB まで）。")
        } else {
            None
        }
    }

    pub(super) fn can_confirm(&self, sending: bool) -> bool {
        matches!(self.phase, ClosePhase::Loaded(_))
            && self.blocker(sending).is_none()
            && self.note_error().is_none()
    }

    /// The command for what is shown now: the revisions and the Ready are the preview's.
    pub(super) fn order(&self, sending: bool) -> Option<CloseLane> {
        let ClosePhase::Loaded(preview) = &self.phase else {
            return None;
        };
        self.can_confirm(sending).then(|| CloseLane {
            lane_id: self.lane.clone(),
            kind: self.choice.kind(),
            ready_seq: preview.ready.as_ref().map(|ready| ready.report_seq),
            expected_management_revision: preview.management_revision,
            expected_request_revision: preview.request_revision,
            note: self.note.trim().to_owned(),
        })
    }

    /// Whether an event for the lane means the preview on screen is no longer the lane.
    pub(super) fn moved_by(
        &self,
        lane: &workbench_protocol::namespaces::lanes::LaneSummary,
    ) -> bool {
        let ClosePhase::Loaded(preview) = &self.phase else {
            return false;
        };
        lane.management_revision != preview.management_revision
            || lane.brief_revision != preview.brief_revision
            || lane.current_close != preview.current_close
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DialogAction {
    Confirm,
    Reload,
    Cancel,
}

fn warning(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(egui::RichText::new(text).color(ui.visuals().warn_fg_color)).wrap());
}

/// Draws the confirmation. It decides nothing: it returns what was pressed.
pub(super) fn draw(
    ctx: &egui::Context,
    dialog: &mut CloseDialog,
    sending: bool,
) -> Option<DialogAction> {
    let mut action = None;
    let modal = egui::Modal::new(egui::Id::new("core-client-lane-close")).show(ctx, |ui| {
        ui.set_width((ctx.viewport_rect().width() * 0.6).clamp(320.0, 640.0));
        ui.heading(dialog.choice.title());
        ui.strong(&dialog.title);
        ui.add(egui::Label::new(MEANING).wrap());
        ui.add(egui::Label::new(REMAINS).wrap());
        if let Some(blocker) = dialog.blocker(sending) {
            warning(ui, &blocker);
        }
        if let ClosePhase::Loaded(preview) = &dialog.phase {
            if preview.running {
                warning(ui, RUNNING_WARNING);
            }
            if dialog.choice == CloseChoice::Complete
                && let Some(text) = ready_warning(preview)
            {
                warning(ui, text);
            }
            if let Some(line) = open_questions_line(preview.open_question_count) {
                warning(ui, &line);
            }
            egui::ScrollArea::vertical()
                .id_salt("lane-close-body")
                .max_height(260.0)
                .show(ui, |ui| {
                    ui.strong("Brief");
                    ui.add(egui::Label::new(&preview.objective).wrap());
                    for criterion in &preview.acceptance_criteria {
                        ui.add(egui::Label::new(format!("• {criterion}")).wrap());
                    }
                    ui.separator();
                    match &preview.ready {
                        Some(ready) => {
                            let for_reference = ready.brief_revision != preview.brief_revision;
                            ui.strong(if for_reference {
                                format!(
                                    "最新の Ready（#{}）— 参考（以前の brief）",
                                    ready.report_seq.0
                                )
                            } else {
                                format!("最新の Ready（#{}）", ready.report_seq.0)
                            });
                            ui.add(egui::Label::new(&ready.summary).wrap());
                            if let Some(revision) = &ready.artifact_revision {
                                ui.label(format!("成果物の revision: {}", revision.value));
                            }
                            for reference in &ready.artifact_refs {
                                ui.add(egui::Label::new(reference).wrap());
                            }
                            let more = usize::try_from(ready.artifact_ref_count)
                                .unwrap_or(usize::MAX)
                                .saturating_sub(ready.artifact_refs.len());
                            if more > 0 {
                                ui.label(format!("ほか {more} 件の参照"));
                            }
                        }
                        None => {
                            ui.strong("Ready の報告なし");
                        }
                    }
                });
        }
        ui.separator();
        ui.label(NOTE_LABEL);
        let edit = egui::ScrollArea::vertical()
            .id_salt("lane-close-note")
            .max_height(96.0)
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut dialog.note)
                        .desired_rows(3)
                        .hint_text("この lane を終了する理由")
                        .desired_width(f32::INFINITY),
                )
            })
            .inner;
        edit.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, NOTE_LABEL)
        });
        if let Some(error) = dialog.note_error().filter(|_| !dialog.note.is_empty()) {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        if let Some(notice) = &dialog.notice {
            ui.add(
                egui::Label::new(egui::RichText::new(notice).color(ui.visuals().error_fg_color))
                    .wrap(),
            );
        }
        ui.label(dialog.choice.recorded_as());
        ui.horizontal(|ui| {
            let can = dialog.can_confirm(sending);
            if ui
                .add_enabled(can, egui::Button::new(dialog.choice.confirm()))
                .clicked()
                && can
            {
                action = Some(DialogAction::Confirm);
            }
            if ui.button("Reload").clicked() {
                action = Some(DialogAction::Reload);
            }
            if ui.button("キャンセル").clicked() {
                action = Some(DialogAction::Cancel);
            }
        });
    });
    if action.is_none() && modal.should_close() {
        action = Some(DialogAction::Cancel);
    }
    action
}
