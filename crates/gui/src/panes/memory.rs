use std::path::{Path, PathBuf};

use storage::memory::{MemoryEntry, MemoryFilter, MemoryStatus};
use storage::{Database, StorageConfig, StorageHandle};

use super::memory_widgets::{
    ALL_STATUSES, APPROVAL_FILTERS, EMPTY, EVIDENCE, NO_PROJECT, REFRESH, approval_action,
    approval_badge, approval_filter_label, reject_controls,
};

pub struct MemoryPane {
    pub config: Option<StorageConfig>,
    /// Writer for approving and rejecting lessons; without it the pane is read-only.
    pub handle: Option<StorageHandle>,
    pub query: String,
    pub status: Option<MemoryStatus>,
    /// `Some(true)` lists only approved (favorite) lessons, `Some(false)` only unconfirmed ones.
    pub approval: Option<bool>,
    root: Option<PathBuf>,
    project: String,
    entries: Vec<MemoryEntry>,
    /// Why the last search failed.
    error: Option<String>,
    /// Why the last approve / clear / reject was refused. Survives the re-fetch that
    /// follows it, so the user sees the reason; the next user action clears it.
    notice: Option<String>,
    /// Lesson awaiting confirmation, since rejection cannot be undone.
    confirm_reject: Option<String>,
    stale: bool,
}

impl Default for MemoryPane {
    /// Opens on the promoted lessons, which are the ones task prompts receive;
    /// the other statuses are a filter away.
    fn default() -> Self {
        Self {
            config: None,
            handle: None,
            query: String::new(),
            status: Some(MemoryStatus::Promoted),
            approval: None,
            root: None,
            project: String::new(),
            entries: Vec::new(),
            error: None,
            notice: None,
            confirm_reject: None,
            stale: false,
        }
    }
}

impl MemoryPane {
    pub fn render(&mut self, ui: &mut egui::Ui, repo_root: Option<&Path>) {
        let Some(repo_root) = repo_root else {
            ui.label(NO_PROJECT);
            return;
        };
        if self.config.is_none() {
            ui.label("Memory storage is not connected.");
            return;
        }
        // `stale` re-fetches after a write; unlike a user-driven refresh it keeps the
        // reason a write was refused on screen.
        let mut refresh = false;
        let after_write = std::mem::take(&mut self.stale);
        if self.root.as_deref() != Some(repo_root) {
            refresh = true;
            self.root = Some(repo_root.to_path_buf());
            self.project = crate::runtime_sink::derive_repo_slug(repo_root);
        }
        let project = self.project.as_str();
        ui.horizontal_wrapped(|ui| {
            ui.label("Search");
            refresh |= ui.text_edit_singleline(&mut self.query).changed();
            egui::ComboBox::from_id_salt("memory_status")
                .selected_text(self.status.map_or(ALL_STATUSES, MemoryStatus::as_str))
                .show_ui(ui, |ui| {
                    refresh |= ui
                        .selectable_value(&mut self.status, None, ALL_STATUSES)
                        .changed();
                    for status in [
                        MemoryStatus::Candidate,
                        MemoryStatus::Validated,
                        MemoryStatus::Promoted,
                        MemoryStatus::Rejected,
                    ] {
                        refresh |= ui
                            .selectable_value(&mut self.status, Some(status), status.as_str())
                            .changed();
                    }
                });
            egui::ComboBox::from_id_salt("memory_approval")
                .selected_text(approval_filter_label(self.approval))
                .show_ui(ui, |ui| {
                    for (value, label) in APPROVAL_FILTERS {
                        refresh |= ui
                            .selectable_value(&mut self.approval, value, label)
                            .changed();
                    }
                });
            refresh |= ui.button(REFRESH).clicked();
        });
        if refresh {
            self.notice = None;
        }
        if refresh || after_write {
            // A confirmation belongs to the list it was shown on.
            self.confirm_reject = None;
            let filter = MemoryFilter {
                status: self.status,
                favorite: self.approval,
            };
            let result = self.config.as_ref().map(|config| {
                Database::open(config)
                    .and_then(|db| db.search_memory_filtered(project, &self.query, filter))
            });
            match result {
                Some(Ok(entries)) => {
                    self.entries = entries;
                    self.error = None;
                }
                Some(Err(error)) => {
                    self.entries.clear();
                    self.error = Some(error.to_string());
                }
                None => {}
            }
        }
        for message in [&self.error, &self.notice].into_iter().flatten() {
            crate::theme::widgets::reason_label(
                ui,
                crate::theme::tokens::palette().ERROR_FG,
                message,
            );
        }
        ui.label(crate::theme::text::muted(format!(
            "{} lessons (up to 100)",
            self.entries.len()
        )));
        let mut reject = None;
        let mut approve = None;
        egui::ScrollArea::vertical()
            .id_salt("memory_results")
            .show(ui, |ui| {
                if self.entries.is_empty() && self.error.is_none() {
                    ui.label(EMPTY);
                }
                for entry in &self.entries {
                    ui.push_id(&entry.lesson.id, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.monospace(&entry.lesson.id);
                            ui.label(entry.status.as_str());
                            ui.label(entry.lesson.scope.as_str());
                            approval_badge(ui, entry.favorite);
                        });
                        ui.label(&entry.lesson.content);
                        ui.collapsing(crate::theme::text::badge(EVIDENCE), |ui| {
                            ui.label(&entry.lesson.evidence);
                            ui.monospace(&entry.lesson.task_id);
                        });
                        if self.handle.is_some() && entry.status != MemoryStatus::Rejected {
                            ui.horizontal(|ui| {
                                let (label, tip) = approval_action(entry.favorite);
                                if ui.button(label).on_hover_text(tip).clicked() {
                                    approve = Some((entry.lesson.id.clone(), !entry.favorite));
                                }
                                // An approved lesson offers no way to reject it.
                                if !entry.favorite {
                                    reject_controls(
                                        ui,
                                        &entry.lesson.id,
                                        &mut self.confirm_reject,
                                        &mut reject,
                                    );
                                }
                            });
                        }
                        ui.separator();
                    });
                }
            });
        if let Some(handle) = &self.handle
            && (reject.is_some() || approve.is_some())
        {
            self.confirm_reject = None;
            self.notice = None;
            let result = match (reject, approve) {
                (Some(id), _) => handle.reject_lesson(&id),
                (None, Some((id, favorite))) => handle.set_lesson_favorite(&id, favorite),
                (None, None) => Ok(()),
            };
            if let Err(error) = result {
                self.notice = Some(error.to_string());
            }
            // Either way the list may be stale (the other path won a race), so re-read it.
            self.stale = true;
            ui.ctx().request_repaint();
        }
    }
}
