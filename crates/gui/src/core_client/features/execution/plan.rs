//! The plan progress of the shown thread. `ConversationPlanChanged` only says
//! that a newer revision exists; the summary is then read again. A notice or an
//! answer for a revision at or below the one known never moves the line back.
use workbench_protocol::{namespaces::execution::plan::PlanSummary, wire::Revision};

use crate::model::conversation_plan::{completed_percent, is_planned};

#[derive(Default)]
pub(super) struct PlanLine {
    summary: Option<PlanSummary>,
    /// The newest revision a notice or an answer reported for the thread.
    latest: u64,
    stale: bool,
}

impl PlanLine {
    /// Forget the old thread's plan; a thread to show is read again.
    pub(super) fn switch(&mut self, shown: bool) {
        *self = Self {
            stale: shown,
            ..Self::default()
        };
    }

    /// The core is asked again after a reconnect. The revision already known
    /// stays, so an answer older than what was shown is still dropped.
    pub(super) fn reset(&mut self, shown: bool) {
        self.stale = shown;
    }

    pub(super) fn changed(&mut self, revision: Revision) {
        if revision.0 > self.latest {
            self.latest = revision.0;
            self.stale = true;
        }
    }

    /// Whether to ask for the summary now; clears the request.
    pub(super) fn take_stale(&mut self) -> bool {
        std::mem::take(&mut self.stale)
    }

    pub(super) fn answered(&mut self, summary: PlanSummary) {
        let shown = self.summary.as_ref().map_or(0, |s| s.revision.0);
        if self.summary.is_some() && summary.revision.0 < shown {
            return;
        }
        self.latest = self.latest.max(summary.revision.0);
        self.summary = Some(summary);
    }

    pub(super) fn draw(&self, ui: &mut egui::Ui) {
        let Some(summary) = &self.summary else {
            return;
        };
        let progress = &summary.progress;
        if !is_planned(progress) {
            let text = if crate::model::conversation_plan::summary_is_cleared(summary) {
                "計画: 消去済み"
            } else {
                "計画: 未計画"
            };
            ui.label(text);
            return;
        }
        let counts = &progress.leaf_counts;
        let mut line = format!(
            "計画: {}% · 完了 {}/{}",
            completed_percent(progress),
            counts.completed,
            progress.leaf_count
        );
        if counts.blocked > 0 {
            line.push_str(&format!(" · ブロック {}", counts.blocked));
        }
        if counts.interrupted > 0 {
            line.push_str(&format!(" · 中断 {}", counts.interrupted));
        }
        ui.label(line);
    }
}

#[cfg(test)]
mod tests;
