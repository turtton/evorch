//! How soon an otherwise idle window asks for its next frame.
//!
//! Most background work wakes the UI itself: the event pump, terminal output, usage loads and
//! browser frames each call `request_repaint*` when they have something to show. A few workers
//! only fill a channel (or finish a thread), and the frame loop notices that by polling in
//! `ui`. For those, an idle window would stay stale, so a frame is requested on a short
//! interval, but only while such work exists. Everything else sits on a slow floor that is just
//! a safety net for throttled timers (persistence syncs, refresh deadlines, receipt repair).

use std::time::{Duration, Instant};

use super::WorkbenchState;
use super::composer_draft::DRAFT_DEBOUNCE;
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
    /// the event pump, terminal PTYs, usage-stat loads, browser screencasts (frames, page
    /// changes and a failed start), HTML snapshots, context previews, thread search, `@`
    /// mentions, file viewer and agent panes (these schedule their own repaint), and the
    /// ownership probe (scheduled on its own).
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
            || self.shared_state_retry_pending()
    }

    /// A sidebar save waiting for another process to release the file, or events of
    /// another process held until the sidebar names their conversation. Neither wakes the
    /// UI; the frame boundary retries them.
    fn shared_state_retry_pending(&self) -> bool {
        self.sidebar_save_deferred() || self.shared_events_held()
    }

    /// When the debounced composer draft is saved, if it is waiting to be. Without a sidebar
    /// file nothing is ever saved and the draft stays dirty, so that is not a reason to wake.
    /// A save that keeps failing is retried after the same debounce, never faster.
    fn draft_save_due_in(&self, now: Instant) -> Option<Duration> {
        self.sidebar_path.as_ref()?;
        let changed = self.draft_dirty_at.get()?;
        let due = (changed + DRAFT_DEBOUNCE).saturating_duration_since(now);
        // A frame past the deadline has just saved it; do not ask for one in the same instant.
        Some(due.max(BUSY_POLL))
    }

    /// The delay to request at the end of a `ui` pass.
    pub(super) fn poll_repaint_delay(&self) -> Duration {
        self.poll_repaint_delay_at(Instant::now())
    }

    fn poll_repaint_delay_at(&self, now: Instant) -> Duration {
        let base = if self.polled_work_pending() {
            BUSY_POLL
        } else {
            IDLE_POLL
        };
        self.draft_save_due_in(now)
            .map_or(base, |due| base.min(due))
    }

    /// The delay a hidden window requests. `logic` only folds events, saves the draft, mirrors
    /// threads and reconciles receipts, so it must not stay awake for results it never collects
    /// (titles, saves, logins and so on wait for the next `ui` pass).
    pub(super) fn logic_repaint_delay(&self) -> Duration {
        self.logic_repaint_delay_at(Instant::now())
    }

    fn logic_repaint_delay_at(&self, now: Instant) -> Duration {
        let base = if self.has_unsettled_follow_ups() || self.shared_state_retry_pending() {
            BUSY_POLL
        } else {
            IDLE_POLL
        };
        self.draft_save_due_in(now)
            .map_or(base, |due| base.min(due))
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

    /// What egui reports for a request: it takes the frame time the pass predicted off the delay.
    fn reported(requested: Duration) -> Duration {
        let predicted = egui::RawInput::default().predicted_dt;
        requested.saturating_sub(Duration::try_from_secs_f32(predicted).unwrap())
    }

    fn assert_requested(delay: Duration, requested: Duration) {
        assert_eq!(delay, reported(requested));
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
    fn a_hidden_window_stays_awake_only_for_what_logic_collects() {
        let mut state = state();
        assert_requested(logic_delay(&mut state), IDLE_POLL);

        // Results only `ui` collects do not keep a hidden window ticking.
        let (_hold, rx) = mpsc::channel();
        state.project_profile_rx = Some(rx);
        assert!(state.polled_work_pending());
        assert_requested(logic_delay(&mut state), IDLE_POLL);
        state.project_profile_rx = None;

        // Receipts are reconciled by `logic`, so they do, and the hidden window rests once settled.
        state
            .follow_up_queues
            .insert("t".into(), vec![queued_follow_up(false)]);
        assert_requested(logic_delay(&mut state), BUSY_POLL);
        state
            .follow_up_queues
            .insert("t".into(), vec![queued_follow_up(true)]);
        assert_requested(logic_delay(&mut state), IDLE_POLL);
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

    fn queued_follow_up(undelivered: bool) -> QueuedFollowUp {
        QueuedFollowUp {
            submission_id: "one".into(),
            text: "one".into(),
            prompt: "one".into(),
            undelivered,
            rejection: None,
        }
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
        state
            .follow_up_queues
            .insert("t".into(), vec![queued_follow_up(true)]);
        assert!(!state.polled_work_pending(), "undelivered rows are settled");
        state
            .follow_up_queues
            .insert("t".into(), vec![queued_follow_up(false)]);
        assert!(state.polled_work_pending());
    }

    #[test]
    fn a_draft_waiting_for_its_debounce_wakes_at_the_save_deadline() {
        let state = state().with_sidebar_path("/nonexistent/sidebar.json".into());
        let typed = Instant::now();
        assert_eq!(state.poll_repaint_delay_at(typed), IDLE_POLL);

        state.draft_dirty_at.set(Some(typed));
        assert!(
            !state.polled_work_pending(),
            "a deadline is not polled work"
        );
        assert_eq!(state.poll_repaint_delay_at(typed), DRAFT_DEBOUNCE);
        assert_eq!(state.logic_repaint_delay_at(typed), DRAFT_DEBOUNCE);
        // Never closer than the busy interval, so a failing save cannot spin the loop.
        assert_eq!(
            state.poll_repaint_delay_at(typed + DRAFT_DEBOUNCE),
            BUSY_POLL
        );

        state.draft_dirty_at.set(None);
        assert_eq!(state.poll_repaint_delay_at(typed), IDLE_POLL);
    }

    #[test]
    fn a_draft_that_is_never_saved_does_not_keep_the_window_awake() {
        let state = state();
        assert!(state.sidebar_path.is_none());
        let typed = Instant::now();
        state.draft_dirty_at.set(Some(typed));
        for elapsed in [Duration::ZERO, DRAFT_DEBOUNCE, DRAFT_DEBOUNCE * 10] {
            assert_eq!(state.poll_repaint_delay_at(typed + elapsed), IDLE_POLL);
            assert_eq!(state.logic_repaint_delay_at(typed + elapsed), IDLE_POLL);
        }
    }

    #[test]
    fn a_deferred_sidebar_save_and_held_shared_events_keep_the_window_polling() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sidebar.json");
        let mut state = crate::app::delivery_tests::state(&path);
        state.save_sidebar();
        assert!(!state.sidebar_save_deferred());
        assert!(!state.polled_work_pending());
        assert_eq!(state.logic_repaint_delay_at(Instant::now()), IDLE_POLL);

        // Another process holds the lock: the save waits and is retried by polling.
        let held = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(directory.path().join("sidebar.json.lock"))
            .unwrap();
        held.lock().unwrap();
        state.save_sidebar();
        assert!(state.sidebar_save_deferred());
        assert!(state.polled_work_pending());
        assert_eq!(state.logic_repaint_delay_at(Instant::now()), BUSY_POLL);

        // The lock is released and the retry succeeds: back to the floor.
        drop(held);
        state.flush_deferred_sidebar_save();
        assert!(!state.polled_work_pending());
        assert_eq!(state.logic_repaint_delay_at(Instant::now()), IDLE_POLL);

        // Events held for a sidebar that does not name their conversation yet.
        state
            .shared_feed
            .pending
            .push_back(crate::shared_state::FedEvent {
                id: 1,
                event: event_bus::Event::new(event_bus::LifecycleEvent::Started {
                    session_id: "held".into(),
                }),
            });
        assert!(state.polled_work_pending());
        assert_eq!(state.logic_repaint_delay_at(Instant::now()), BUSY_POLL);
    }
}
