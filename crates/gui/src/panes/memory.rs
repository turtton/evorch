use std::path::{Path, PathBuf};

use storage::memory::{MemoryEntry, MemoryStatus};
use storage::{Database, StorageConfig, StorageHandle};

pub struct MemoryPane {
    pub config: Option<StorageConfig>,
    /// Writer for rejecting lessons; without it the pane is read-only.
    pub handle: Option<StorageHandle>,
    pub query: String,
    pub status: Option<MemoryStatus>,
    root: Option<PathBuf>,
    project: String,
    entries: Vec<MemoryEntry>,
    error: Option<String>,
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
            root: None,
            project: String::new(),
            entries: Vec::new(),
            error: None,
            confirm_reject: None,
            stale: false,
        }
    }
}

impl MemoryPane {
    pub fn render(&mut self, ui: &mut egui::Ui, repo_root: Option<&Path>) {
        let Some(repo_root) = repo_root else {
            ui.label("Select a project to browse memory.");
            return;
        };
        if self.config.is_none() {
            ui.label("Memory storage is not connected.");
            return;
        }
        let mut refresh = std::mem::take(&mut self.stale);
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
                .selected_text(self.status.map_or("All statuses", MemoryStatus::as_str))
                .show_ui(ui, |ui| {
                    refresh |= ui
                        .selectable_value(&mut self.status, None, "All statuses")
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
            refresh |= ui.button("Refresh").clicked();
        });
        if refresh {
            let result = self.config.as_ref().map(|config| {
                Database::open(config)
                    .and_then(|db| db.search_memory(project, &self.query, self.status))
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
        if let Some(error) = &self.error {
            ui.colored_label(crate::theme::tokens::palette().ERROR_FG, error);
        }
        ui.label(crate::theme::text::muted(format!(
            "{} lessons (up to 100)",
            self.entries.len()
        )));
        let mut reject = None;
        egui::ScrollArea::vertical()
            .id_salt("memory_results")
            .show(ui, |ui| {
                if self.entries.is_empty() && self.error.is_none() {
                    ui.label("No matching lessons.");
                }
                for entry in &self.entries {
                    ui.push_id(&entry.lesson.id, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.monospace(&entry.lesson.id);
                            ui.label(entry.status.as_str());
                            ui.label(entry.lesson.scope.as_str());
                        });
                        ui.label(&entry.lesson.content);
                        ui.collapsing(crate::theme::text::badge("Evidence"), |ui| {
                            ui.label(&entry.lesson.evidence);
                            ui.monospace(&entry.lesson.task_id);
                        });
                        if self.handle.is_some() && entry.status != MemoryStatus::Rejected {
                            ui.horizontal(|ui| {
                                if self.confirm_reject.as_ref() == Some(&entry.lesson.id) {
                                    ui.label("Reject permanently?");
                                    if ui.button("Confirm reject").clicked() {
                                        reject = Some(entry.lesson.id.clone());
                                    }
                                    if ui.button("Cancel").clicked() {
                                        self.confirm_reject = None;
                                    }
                                } else if ui
                                    .button("Reject")
                                    .on_hover_text(
                                        "Stop using this lesson in task prompts. Cannot be undone.",
                                    )
                                    .clicked()
                                {
                                    self.confirm_reject = Some(entry.lesson.id.clone());
                                }
                            });
                        }
                        ui.separator();
                    });
                }
            });
        if let Some(id) = reject
            && let Some(handle) = &self.handle
        {
            self.confirm_reject = None;
            match handle.reject_lesson(&id) {
                Ok(()) => self.stale = true,
                Err(error) => self.error = Some(error.to_string()),
            }
            ui.ctx().request_repaint();
        }
    }
}
