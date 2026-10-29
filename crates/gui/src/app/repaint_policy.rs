//! How soon an otherwise idle window asks for its next frame.
//!
//! Most background work wakes the UI itself: the event pump, terminal output, usage loads and
//! browser frames each call `request_repaint*` when they have something to show. A few workers
//! only fill a channel (or finish a thread), and the frame loop notices that by polling in
//! `ui`. For those, an idle window would stay stale, so a frame is requested on a short
//! interval, but only while such work exists. Everything else sits on a slow floor that is just
//! a safety net for throttled timers (persistence syncs, refresh deadlines, receipt repair).

use std::time::Duration;

use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;

/// Interval while a worker that cannot wake the UI is still running.
pub(super) const BUSY_POLL: Duration = Duration::from_millis(200);
/// Interval when nothing is running that only polling can observe.
pub(super) const IDLE_POLL: Duration = Duration::from_secs(1);

impl<S: AgentRunSource> WorkbenchState<S> {
    /// A queued follow-up that no receipt has settled yet. The frame boundary re-reads receipts
    /// for it, since a delivery can be consumed without any event reaching the pump.
    pub(super) fn has_unsettled_follow_ups(&self) -> bool {
        self.follow_up_queues
            .values()
            .any(|rows| rows.iter().any(|row| !row.undelivered))
    }

    /// Whether some worker finishes without waking the UI, so only a poll in `ui` observes it.
    ///
    /// This is the audit of every poll `ui` makes. Sources that notify are deliberately absent:
    /// the event pump, terminal PTYs, usage-stat loads, browser screencasts, HTML snapshots,
    /// context previews, thread search, `@` mentions, file viewer and agent panes (these
    /// schedule their own repaint), and the ownership probe (scheduled on its own).
    pub(super) fn polled_work_pending(&self) -> bool {
        self.external_command_running()
            || !self.auto_title_jobs.is_empty()
            || self.sink.has_background_work()
            || self.diff.is_busy()
            || self.settings_save_in_progress()
            || self.project_role_profile_pending()
            || self.storage_settings.is_busy()
            || self.folder_picker.is_busy()
            || self.provider_settings.has_background_work()
            || self.codex_auth().is_authenticating()
            || self.telemetry.quota.is_busy()
            || self.telemetry.kimi_quota.state.is_busy()
            || self.telemetry.subscription_quota.claude.is_busy()
            || self.telemetry.subscription_quota.cursor.is_busy()
            || self.has_unsettled_follow_ups()
            // A draft waiting out its debounce is saved by a later frame, not by an event.
            || self.draft_dirty_at.get().is_some()
    }

    /// The delay to request at the end of a pass.
    pub(super) fn poll_repaint_delay(&self) -> Duration {
        if self.polled_work_pending() {
            BUSY_POLL
        } else {
            IDLE_POLL
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, mpsc};

    use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

    use super::*;
    use crate::fixture::DemoSource;
    use crate::model::commands::{CommandSink, LoopEvent, WorkbenchCommand};
    use crate::model::composer::QueuedFollowUp;
    use crate::model::folder_picker::FolderPicker;
    use crate::model::provider_settings::{OpenAiEditorModel, ProfileEditor};

    type State = WorkbenchState<DemoSource>;

    fn state() -> State {
        let mut sidebar = SidebarState::default();
        let project = ProjectId::new("p");
        sidebar
            .add_project(project.clone(), "p", std::path::Path::new("/"))
            .unwrap();
        sidebar.select_project(&project).unwrap();
        sidebar
            .create_thread(ThreadId::new("t"), project, "t")
            .unwrap();
        sidebar.switch_thread(&ThreadId::new("t")).unwrap();
        WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
            .unwrap()
            .with_sidebar(sidebar)
    }

    /// egui reports a request minus the predicted frame time of the pass that made it.
    const FRAME_SLACK: Duration = Duration::from_millis(50);

    fn assert_requested(delay: Duration, expected: Duration) {
        assert!(
            delay <= expected && delay + FRAME_SLACK > expected,
            "expected about {expected:?}, egui reports {delay:?}"
        );
    }

    /// One `ui` pass; the delay egui reports for the next frame.
    fn ui_delay(state: &mut State, ctx: &egui::Context) -> Duration {
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            state.ui(ui, &mut eframe::Frame::_new_kittest());
        });
        output.textures_delta.clear();
        output.viewport_output[&egui::ViewportId::ROOT].repaint_delay
    }

    /// A fresh context owes a few immediate frames; pass through them to reach rest.
    fn settled(state: &mut State) -> (egui::Context, Duration) {
        let ctx = egui::Context::default();
        let mut delay = Duration::ZERO;
        for _ in 0..4 {
            delay = ui_delay(state, &ctx);
        }
        (ctx, delay)
    }

    #[test]
    fn an_idle_window_asks_for_a_frame_once_a_second() {
        let mut state = state();
        assert!(!state.polled_work_pending());
        let (_, delay) = settled(&mut state);
        assert_requested(delay, IDLE_POLL);
    }

    #[test]
    fn a_window_with_polled_work_keeps_the_short_interval_until_it_ends() {
        let mut state = state();
        let (hold, rx) = mpsc::channel();
        state.project_profile_rx = Some(rx);
        let (ctx, delay) = settled(&mut state);
        assert_requested(delay, BUSY_POLL);

        // The recomposition finishes: the next pass collects it and the floor returns.
        hold.send(Ok(())).unwrap();
        let mut delay = Duration::ZERO;
        for _ in 0..4 {
            delay = ui_delay(&mut state, &ctx);
        }
        assert!(!state.polled_work_pending());
        assert_requested(delay, IDLE_POLL);
    }

    /// The delay `logic` asks for, as the repaint callback sees it (a hidden window has no pass
    /// output to read it from).
    fn logic_delay(state: &mut State) -> Duration {
        let ctx = egui::Context::default();
        // Rest first, or the callback only reports the immediate frame a new context owes.
        loop {
            let mut output = ctx.run_ui(egui::RawInput::default(), |_ui| {});
            output.textures_delta.clear();
            if output.viewport_output[&egui::ViewportId::ROOT].repaint_delay == Duration::MAX {
                break;
            }
        }
        let requested = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&requested);
        ctx.set_request_repaint_callback(move |info| sink.lock().unwrap().push(info.delay));
        let _ = ctx.run_logic(&egui::RawInput::default(), |ctx| state.logic(ctx));
        let requested = requested.lock().unwrap();
        requested
            .iter()
            .copied()
            .min()
            .expect("logic asks for a repaint")
    }

    #[test]
    fn a_hidden_window_follows_the_same_rule() {
        let mut state = state();
        assert_requested(logic_delay(&mut state), IDLE_POLL);

        let (_hold, rx) = mpsc::channel();
        state.routing_settings.save_rx = Some(rx);
        assert_requested(logic_delay(&mut state), BUSY_POLL);
    }

    #[test]
    fn an_external_command_is_polled_until_it_ends() {
        let mut state = state();
        state.start_external(std::future::pending());
        assert!(state.polled_work_pending());
        // Dropping the job cancels the worker.
        state.external_job = None;
        assert!(!state.polled_work_pending());
    }

    #[test]
    fn a_title_being_generated_is_polled() {
        let mut state = state();
        let (job, _result) = super::super::auto_title::Job::pending(ThreadId::new("t"));
        state.auto_title_jobs.push(job);
        assert!(state.polled_work_pending());
        state.auto_title_jobs.clear();
        assert!(!state.polled_work_pending());
    }

    struct Background(bool);

    impl CommandSink for Background {
        fn submit(&mut self, _: WorkbenchCommand) -> Vec<LoopEvent> {
            Vec::new()
        }
        fn has_background_work(&self) -> bool {
            self.0
        }
    }

    #[test]
    fn a_command_sink_job_is_polled() {
        let mut state = state();
        state.sink = Box::new(Background(true));
        assert!(state.polled_work_pending());
        state.sink = Box::new(Background(false));
        assert!(!state.polled_work_pending());
    }

    #[test]
    fn every_settings_save_is_polled() {
        let mut state = state();
        let (_hold, rx) = mpsc::channel();
        state.routing_settings.save_rx = Some(rx);
        assert!(state.polled_work_pending());
        state.routing_settings.save_rx = None;

        let (_hold, rx) = mpsc::channel();
        state.role_settings.save_rx = Some(rx);
        assert!(state.polled_work_pending());
        state.role_settings.save_rx = None;

        let (_hold, rx) = mpsc::channel();
        state.provider_save_rx = Some(rx);
        assert!(state.polled_work_pending());
        state.provider_save_rx = None;

        let (_hold, rx) = mpsc::channel();
        state.project_profile_rx = Some(rx);
        assert!(state.polled_work_pending());
        state.project_profile_rx = None;
        assert!(!state.polled_work_pending());
    }

    #[test]
    fn a_pending_folder_dialog_is_polled() {
        type Pick = Result<Option<std::path::PathBuf>, String>;
        struct Held(Mutex<Vec<mpsc::Sender<Pick>>>);
        impl FolderPicker for Held {
            fn pick(&self) -> mpsc::Receiver<Pick> {
                let (tx, rx) = mpsc::channel();
                self.0.lock().unwrap().push(tx);
                rx
            }
        }
        let mut state = state().with_folder_picker(Arc::new(Held(Mutex::default())));
        assert!(!state.polled_work_pending());
        state.folder_picker.start().unwrap();
        assert!(state.polled_work_pending());
    }

    #[test]
    fn an_open_editor_fetching_models_is_polled() {
        let mut state = state();
        state.provider_settings.editor =
            Some(ProfileEditor::OpenAiCompatible(OpenAiEditorModel::default()));
        assert!(!state.polled_work_pending());
        let (_hold, rx) = mpsc::channel();
        state
            .provider_settings
            .openai_mut()
            .expect("openai editor")
            .models_rx = Some(rx);
        assert!(state.polled_work_pending());
    }

    #[test]
    fn a_queued_follow_up_is_polled_until_it_settles() {
        let mut state = state();
        let row = |undelivered| QueuedFollowUp {
            submission_id: "one".into(),
            text: "one".into(),
            prompt: "one".into(),
            undelivered,
            rejection: None,
        };
        state.follow_up_queues.insert("t".into(), vec![row(true)]);
        assert!(!state.polled_work_pending(), "undelivered rows are settled");
        state.follow_up_queues.insert("t".into(), vec![row(false)]);
        assert!(state.polled_work_pending());
    }

    #[test]
    fn a_draft_waiting_for_its_debounce_is_polled() {
        let state = state();
        state.draft_dirty_at.set(Some(std::time::Instant::now()));
        assert!(state.polled_work_pending());
        state.draft_dirty_at.set(None);
        assert!(!state.polled_work_pending());
    }
}
