//! Typing updates memory immediately, but only idle input triggers disk I/O.
use std::time::{Duration, Instant};

use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;

pub(super) const DRAFT_DEBOUNCE: Duration = Duration::from_millis(750);

impl<S: AgentRunSource> WorkbenchState<S> {
    pub(super) fn record_composer_draft(&mut self, now: Instant) {
        let Some(thread) = self
            .sidebar
            .threads
            .iter_mut()
            .find(|thread| Some(&thread.id) == self.sidebar.active_thread.as_ref())
        else {
            return;
        };
        if thread.draft_input != self.composer.input {
            thread.draft_input.clone_from(&self.composer.input);
            self.draft_dirty_at.set(Some(now));
        }
    }

    pub(super) fn persist_composer_draft_at(&mut self, now: Instant) {
        self.record_composer_draft(now);
        if self
            .draft_dirty_at
            .get()
            .is_some_and(|changed| now.saturating_duration_since(changed) >= DRAFT_DEBOUNCE)
        {
            self.save_sidebar();
            // A failed atomic save keeps the draft dirty, but retries are also
            // debounced so an unavailable path cannot cause per-frame disk I/O.
            if self.draft_dirty_at.get().is_some() {
                self.draft_dirty_at.set(Some(now));
            }
        }
    }

    pub(super) fn persist_composer_draft(&mut self) {
        self.persist_composer_draft_at(Instant::now());
    }

    /// Drain may take many frames. Force the first save, then reuse the dirty
    /// debounce/retry path instead of writing unchanged history on every tick.
    pub(super) fn flush_shutdown_draft_at(&mut self, now: Instant) {
        if !self.shutdown_draft_flushed {
            self.shutdown_draft_flushed = true;
            self.record_composer_draft(now);
            self.draft_dirty_at.set(Some(now));
            self.save_sidebar();
        } else {
            self.persist_composer_draft_at(now);
        }
    }

    /// Operation/shutdown boundary: include input not yet observed by a UI frame.
    pub fn flush_composer_draft(&mut self) {
        self.record_composer_draft(Instant::now());
        self.save_sidebar();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::delivery_tests::state;
    use workspace_ui::{ProjectId, ThreadId};

    fn saved_draft(path: &std::path::Path) -> String {
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        json["threads"][0]["draft_input"]
            .as_str()
            .unwrap_or_default()
            .into()
    }

    #[test]
    fn typing_debounces_one_atomic_save_and_later_saves_do_not_repeat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sidebar.json");
        let mut state = state(&path);
        let now = Instant::now();
        for i in 0..10 {
            state.composer.input.push('a');
            state.persist_composer_draft_at(now + Duration::from_millis(i * 100));
        }
        assert_eq!(state.sidebar_write_count.get(), 0);
        assert!(!path.exists());
        state.persist_composer_draft_at(now + Duration::from_millis(1649));
        assert_eq!(state.sidebar_write_count.get(), 0);
        state.persist_composer_draft_at(now + Duration::from_millis(1650));
        assert_eq!(state.sidebar_write_count.get(), 1);
        assert_eq!(saved_draft(&path), "aaaaaaaaaa");
        state.persist_composer_draft_at(now + Duration::from_secs(10));
        assert_eq!(state.sidebar_write_count.get(), 1);
    }

    #[test]
    fn switching_submitting_and_shutdown_flush_without_waiting_for_debounce() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sidebar.json");
        let mut state = state(&path);
        state
            .sidebar
            .create_thread(ThreadId::new("other"), ProjectId::new("test"), "other")
            .unwrap();
        state.composer.input = "switch draft".into(); // no intervening frame
        state.switch_thread(ThreadId::new("other")).unwrap();
        assert_eq!(saved_draft(&path), "switch draft");
        assert!(state.draft_dirty_at.get().is_none());
        state.switch_thread(ThreadId::new("thread")).unwrap();
        state.submit_composer();
        assert!(state.composer.input.is_empty());
        assert_eq!(saved_draft(&path), "");
        state.composer.input = "shutdown draft".into();
        state.flush_composer_draft(); // production Drop/on_exit/quiesce boundary
        assert_eq!(saved_draft(&path), "shutdown draft");
        assert!(state.draft_dirty_at.get().is_none());
    }

    #[test]
    fn failed_atomic_save_retains_dirty_input_and_retries_after_debounce() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing/sidebar.json");
        let mut state = state(&dir.path().join("sidebar.json"));
        state.sidebar_path = Some(path.clone());
        state.composer.input = "keep me".into();
        let now = Instant::now();
        state.persist_composer_draft_at(now);
        state.persist_composer_draft_at(now + DRAFT_DEBOUNCE);
        assert!(state.draft_dirty_at.get().is_some());
        assert_eq!(state.sidebar_write_count.get(), 0);
        std::fs::create_dir(path.parent().unwrap()).unwrap();
        state.persist_composer_draft_at(now + DRAFT_DEBOUNCE + Duration::from_millis(1));
        assert!(!path.exists());
        state.persist_composer_draft_at(now + DRAFT_DEBOUNCE * 2);
        assert_eq!(saved_draft(&path), "keep me");
        assert!(state.draft_dirty_at.get().is_none());
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use crate::app::delivery_tests::state;

    #[test]
    fn drain_wait_saves_once_then_only_dirty_changes_and_failed_save_retries() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sidebar.json");
        let mut state = state(&path);
        let now = Instant::now();
        state.composer.input = "first".into();
        state.flush_shutdown_draft_at(now);
        for n in 1..100 {
            state.flush_shutdown_draft_at(now + Duration::from_millis(n * 100));
        }
        assert_eq!(state.sidebar_write_count.get(), 1);
        let edit = now + Duration::from_secs(10);
        state.composer.input = "changed during drain".into();
        state.flush_shutdown_draft_at(edit);
        state.flush_shutdown_draft_at(edit + DRAFT_DEBOUNCE);
        assert_eq!(state.sidebar_write_count.get(), 2);

        state.shutdown_draft_flushed = false;
        let unavailable = directory.path().join("missing/sidebar.json");
        state.sidebar_path = Some(unavailable.clone());
        state.flush_shutdown_draft_at(edit + Duration::from_secs(1));
        assert!(state.draft_dirty_at.get().is_some());
        std::fs::create_dir(unavailable.parent().unwrap()).unwrap();
        state.flush_shutdown_draft_at(edit + Duration::from_secs(1) + DRAFT_DEBOUNCE);
        assert_eq!(state.sidebar_write_count.get(), 3);
        assert!(state.draft_dirty_at.get().is_none());
        state.flush_shutdown_draft_at(edit + Duration::from_secs(5));
        assert_eq!(state.sidebar_write_count.get(), 3);
    }
}

#[cfg(test)]
mod switch_failure_tests {
    use super::*;
    use crate::app::delivery_tests::state;
    use workspace_ui::{ProjectId, ThreadId};

    fn retries_unobserved_draft_after_boundary_failure(archive: bool, next: bool) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unavailable/sidebar.json");
        let mut state = state(&directory.path().join("sidebar.json"));
        state.sidebar_path = Some(path.clone());
        if next {
            state
                .sidebar
                .create_thread(ThreadId::new("next"), ProjectId::new("test"), "next")
                .unwrap();
            state.sidebar.threads[1].draft_input = "keep next".into();
        }
        state.composer.input = "unobserved old input".into();
        assert!(state.draft_dirty_at.get().is_none());
        if archive {
            state.toggle_archive(ThreadId::new("thread")).unwrap();
        } else {
            state.switch_thread(ThreadId::new("next")).unwrap();
        }
        assert!(state.draft_dirty_at.get().is_some());
        assert_eq!(state.sidebar_write_count.get(), 0);
        assert!(!path.exists());
        assert_eq!(state.composer.input, if next { "keep next" } else { "" });
        std::fs::create_dir(path.parent().unwrap()).unwrap();
        // Only a normal timer tick after recovery; no typing, selection or flush.
        state.persist_composer_draft_at(Instant::now() + DRAFT_DEBOUNCE);
        let saved = workspace_ui::load_sidebar(&path).unwrap();
        assert_eq!(saved.threads[0].draft_input, "unobserved old input");
        if next {
            assert_eq!(saved.threads[1].draft_input, "keep next");
            assert_eq!(saved.active_thread, Some(ThreadId::new("next")));
        } else {
            assert!(saved.active_thread.is_none());
        }
        assert!(state.draft_dirty_at.get().is_none());
        assert_eq!(state.sidebar_write_count.get(), 1);
    }

    #[test]
    fn ordinary_switch_retries_unobserved_draft_after_save_failure() {
        retries_unobserved_draft_after_boundary_failure(false, true);
    }
    #[test]
    fn archive_switch_retries_unobserved_draft_after_save_failure() {
        retries_unobserved_draft_after_boundary_failure(true, true);
    }
    #[test]
    fn archive_last_thread_retries_unobserved_draft_after_save_failure() {
        retries_unobserved_draft_after_boundary_failure(true, false);
    }
}
