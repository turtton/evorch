use std::path::{Path, PathBuf};

use storage::memory::{MemoryEntry, MemoryFilter, MemoryStatus};
use storage::{Database, StorageConfig, StorageHandle};

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
            ui.label("Select a project to browse memory.");
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
            egui::ComboBox::from_id_salt("memory_approval")
                .selected_text(match self.approval {
                    None => "Any approval",
                    Some(true) => "Approved",
                    Some(false) => "Unconfirmed",
                })
                .show_ui(ui, |ui| {
                    for (value, label) in [
                        (None, "Any approval"),
                        (Some(true), "Approved"),
                        (Some(false), "Unconfirmed"),
                    ] {
                        refresh |= ui
                            .selectable_value(&mut self.approval, value, label)
                            .changed();
                    }
                });
            refresh |= ui.button("Refresh").clicked();
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
                    ui.label("No matching lessons.");
                }
                for entry in &self.entries {
                    ui.push_id(&entry.lesson.id, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.monospace(&entry.lesson.id);
                            ui.label(entry.status.as_str());
                            ui.label(entry.lesson.scope.as_str());
                            let (badge, tip) = if entry.favorite {
                                (
                                    "Approved",
                                    "You approved this lesson (favorite). This mark is separate \
                                     from the status and cannot be rejected until you clear it.",
                                )
                            } else {
                                (
                                    "Unconfirmed",
                                    "You have not approved this lesson. Approval is your own \
                                     mark, separate from the status.",
                                )
                            };
                            ui.label(crate::theme::text::badge(badge)).on_hover_text(tip);
                        });
                        ui.label(&entry.lesson.content);
                        ui.collapsing(crate::theme::text::badge("Evidence"), |ui| {
                            ui.label(&entry.lesson.evidence);
                            ui.monospace(&entry.lesson.task_id);
                        });
                        if self.handle.is_some() && entry.status != MemoryStatus::Rejected {
                            ui.horizontal(|ui| {
                                let (label, tip) = if entry.favorite {
                                    (
                                        "Clear approval",
                                        "Remove your approval mark. The lesson's status is unchanged.",
                                    )
                                } else {
                                    (
                                        "Approve",
                                        "Mark this lesson as approved by you. The status is unchanged, \
                                         and an approved lesson cannot be rejected.",
                                    )
                                };
                                if ui.button(label).on_hover_text(tip).clicked() {
                                    approve = Some((entry.lesson.id.clone(), !entry.favorite));
                                }
                                // An approved lesson offers no way to reject it.
                                if !entry.favorite {
                                    reject_controls(ui, entry, &mut self.confirm_reject, &mut reject);
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

/// Reject button, or its confirmation once clicked, since rejection cannot be undone.
fn reject_controls(
    ui: &mut egui::Ui,
    entry: &MemoryEntry,
    confirm: &mut Option<String>,
    reject: &mut Option<String>,
) {
    if confirm.as_ref() == Some(&entry.lesson.id) {
        ui.label("Reject permanently?");
        if ui.button("Confirm reject").clicked() {
            *reject = Some(entry.lesson.id.clone());
        }
        if ui.button("Cancel").clicked() {
            *confirm = None;
        }
    } else if ui
        .button("Reject")
        .on_hover_text("Stop using this lesson in task prompts. Cannot be undone.")
        .clicked()
    {
        *confirm = Some(entry.lesson.id.clone());
    }
}
