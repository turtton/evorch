//! The window shown while the process takes its locks and the start-up
//! maintenance runs (see [`crate::startup`]): it lists each stage as it starts and
//! finishes, counts the families whose deletion was confirmed, and shows the time
//! spent and any failure.

use std::time::{Duration, Instant};

use runtime::thread_retention::{Stage, StartupProgress};

use crate::startup::StartupEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageState {
    Running,
    Finished,
}

/// What the splash knows so far.
#[derive(Debug)]
pub struct Splash {
    started: Instant,
    message: String,
    stages: Vec<(String, StageState)>,
    confirmed_families: usize,
    error: Option<String>,
    /// The workbench is being built; the splash is on screen for this one frame.
    restoring: bool,
}

impl Default for Splash {
    fn default() -> Self {
        Self::new()
    }
}

/// The name of a stage. A stage this build does not know is shown by its debug
/// name, so adding one to the phase never breaks the splash.
#[allow(unreachable_patterns)]
fn stage_label(stage: Stage) -> String {
    match stage {
        Stage::Backfill => "イベントの帰属を整理".to_owned(),
        Stage::Explore => "削除対象を探索".to_owned(),
        Stage::Finalize => "削除を確定".to_owned(),
        Stage::Owners => "所有情報を整理".to_owned(),
        Stage::Scratch => "作業ディレクトリを削除".to_owned(),
        Stage::Oversized => "大きな行を削除".to_owned(),
        Stage::Vacuum => "空き領域を回収".to_owned(),
        _ => format!("{stage:?}"),
    }
}

impl Splash {
    #[must_use]
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            message: "ロックを取得しています…".to_owned(),
            stages: Vec::new(),
            confirmed_families: 0,
            error: None,
            restoring: false,
        }
    }

    pub fn apply(&mut self, event: &StartupEvent) {
        match event {
            StartupEvent::ExclusiveTaken => {
                "サイドバーを読み込んでいます…".clone_into(&mut self.message)
            }
            StartupEvent::ExclusiveUnavailable => {
                "別のウィンドウが動作中のため、起動時の保守は行いません。"
                    .clone_into(&mut self.message);
            }
            StartupEvent::SidebarLoaded { threads } => {
                self.message = format!("{threads} 件のスレッドを確認しています…");
            }
            StartupEvent::Phase(progress) => self.apply_progress(progress),
            StartupEvent::Shared => {
                "共有ロックに切り替えました。".clone_into(&mut self.message);
            }
        }
    }

    fn apply_progress(&mut self, progress: &StartupProgress) {
        match progress {
            StartupProgress::StageStarted(stage) => {
                self.stages.push((stage_label(*stage), StageState::Running));
            }
            StartupProgress::StageFinished(stage) => {
                let label = stage_label(*stage);
                if let Some((_, state)) = self
                    .stages
                    .iter_mut()
                    .rev()
                    .find(|(name, state)| *name == label && *state == StageState::Running)
                {
                    *state = StageState::Finished;
                }
            }
            StartupProgress::FamilyConfirmed { .. } => self.confirmed_families += 1,
            StartupProgress::RootBound => {}
        }
    }

    /// The start-up is over and the workbench is being restored.
    pub fn restoring(&mut self) {
        self.restoring = true;
        "履歴を復元しています…".clone_into(&mut self.message);
    }

    pub fn fail(&mut self, error: impl Into<String>) {
        self.error = Some(error.into());
    }

    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    #[must_use]
    pub const fn is_restoring(&self) -> bool {
        self.restoring
    }

    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Draws the splash. Elapsed time changes by itself, so a repaint is asked for.
    pub fn ui(&self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.25);
            ui.heading("evorch");
            ui.add_space(12.0);
            ui.label(&self.message);
            ui.add_space(12.0);
            for (name, state) in &self.stages {
                let mark = match state {
                    StageState::Running => "●",
                    StageState::Finished => "✓",
                };
                ui.label(format!("{mark} {name}"));
            }
            if self.confirmed_families > 0 {
                ui.add_space(8.0);
                ui.label(format!("削除を確定した系統: {}", self.confirmed_families));
            }
            ui.add_space(8.0);
            ui.label(format!("経過 {:.1} 秒", self.elapsed().as_secs_f32()));
            if let Some(error) = &self.error {
                ui.add_space(12.0);
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    format!("起動に失敗しました: {error}"),
                );
            }
        });
        if self.error.is_none() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use egui_kittest::{Harness, kittest::Queryable};

    use super::*;

    fn started(stage: Stage) -> StartupEvent {
        StartupEvent::Phase(StartupProgress::StageStarted(stage))
    }

    fn finished(stage: Stage) -> StartupEvent {
        StartupEvent::Phase(StartupProgress::StageFinished(stage))
    }

    #[test]
    fn every_stage_the_phase_reports_has_a_name_of_its_own() {
        for stage in [
            Stage::Backfill,
            Stage::Explore,
            Stage::Finalize,
            Stage::Owners,
            Stage::Scratch,
            Stage::Oversized,
            Stage::Vacuum,
        ] {
            assert_ne!(stage_label(stage), format!("{stage:?}"), "{stage:?}");
        }
    }

    #[test]
    fn stages_appear_in_the_order_they_start_and_the_finished_ones_are_ticked() {
        // Given: a splash that saw the first stage finish and the second start.
        let mut splash = Splash::new();
        for event in [
            StartupEvent::ExclusiveTaken,
            started(Stage::Backfill),
            finished(Stage::Backfill),
            started(Stage::Explore),
            StartupEvent::Phase(StartupProgress::FamilyConfirmed {
                family_key: "f".into(),
            }),
        ] {
            splash.apply(&event);
        }

        // When: it is drawn.
        let mut harness = Harness::new_ui(|ui| splash.ui(ui));
        harness.run_steps(2);

        // Then: finished and running stages are marked, and the confirmed count shows.
        harness.get_by_label("✓ イベントの帰属を整理");
        harness.get_by_label("● 削除対象を探索");
        harness.get_by_label("削除を確定した系統: 1");
        assert!(harness.query_by_label_contains("確定").is_some());
        assert!(harness.query_by_label_contains("起動に失敗").is_none());
    }

    #[test]
    fn a_failure_is_shown_and_the_restore_message_replaces_the_progress_line() {
        let mut splash = Splash::new();
        splash.restoring();
        assert!(splash.is_restoring());
        splash.fail("database is locked");
        assert_eq!(splash.error(), Some("database is locked"));

        let mut harness = Harness::new_ui(|ui| splash.ui(ui));
        harness.run_steps(2);

        harness.get_by_label("履歴を復元しています…");
        harness.get_by_label_contains("起動に失敗しました: database is locked");
    }
}
