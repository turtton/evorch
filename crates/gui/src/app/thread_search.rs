//! Thread search modal: opening it, running its searches and opening a result.

use crate::model::tasks::AgentRunSource;
use crate::panes::thread_search::{ThreadSearchAction, thread_search_modal};

use super::WorkbenchState;

/// How soon to look again at a running search; the sink has no way to wake the UI.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Opens the search for the project selected now (the Ctrl+K target),
    /// unless another modal owns input: two modals never stack. The search
    /// keeps that project even if the selection changes while it is open.
    pub fn open_thread_search(&mut self) {
        let selected = self.sidebar.selected_project.clone();
        self.open_thread_search_for(selected);
    }

    /// Opens the search for `project`, the one a sidebar row names. Neither the
    /// selected project nor the active thread changes.
    pub fn open_thread_search_in(&mut self, project: workspace_ui::ProjectId) {
        self.open_thread_search_for(Some(project));
    }

    /// Ctrl+K: closes the open search, or opens it for the selected project.
    pub(super) fn toggle_thread_search(&mut self) {
        if self.thread_search.is_open() {
            self.thread_search.close();
        } else {
            self.open_thread_search();
        }
    }

    fn open_thread_search_for(&mut self, project: Option<workspace_ui::ProjectId>) {
        if self.thread_search_available() {
            self.thread_search.open(project);
        }
    }

    pub fn thread_search(&self) -> &crate::model::thread_search::ThreadSearchModal {
        &self.thread_search
    }

    /// Whether the search shortcut can act here: not while another modal owns input.
    pub(super) fn thread_search_available(&self) -> bool {
        !self.settings_owns_input() && !self.picker_owns_input() && !self.project_dialog.is_open()
    }

    /// Claims the modal's keys ahead of every pane, so Esc closes the search
    /// instead of stopping the chat behind it.
    pub(super) fn capture_thread_search_keys(&mut self, ctx: &egui::Context) {
        self.thread_search.capture_keys(ctx);
    }

    pub(super) fn render_thread_search(&mut self, ctx: &egui::Context) {
        if !self.thread_search.is_open() {
            return;
        }
        // The model picker is exclusive: if it came up, the search gives way.
        if self.picker_owns_input() {
            self.thread_search.close();
            return;
        }
        // The project fixed when the modal opened, not the current selection.
        let project = self.thread_search.root(&self.sidebar);
        let mut pending = self
            .thread_search
            .poll(&*self.sink, project.as_deref(), &self.sidebar);
        let action = thread_search_modal(
            ctx,
            &mut self.thread_search,
            &self.sidebar,
            project.as_deref(),
        );
        // Text typed this frame starts its search now, not a frame later.
        pending |= self
            .thread_search
            .poll(&*self.sink, project.as_deref(), &self.sidebar);
        if pending {
            ctx.request_repaint_after(POLL_INTERVAL);
        }
        match action {
            Some(ThreadSearchAction::Open(thread)) => {
                self.thread_search.close();
                if let Err(error) = self.switch_thread(thread) {
                    self.push_notice(format!("Could not open thread: {error}"));
                }
            }
            Some(ThreadSearchAction::Close) => self.thread_search.close(),
            None => {}
        }
    }
}
