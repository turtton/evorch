use storage::memory::{MemoryEntry, MemoryStatus};
use storage::{Database, StorageConfig};

#[derive(Default)]
pub struct MemoryPane {
    pub config: Option<StorageConfig>,
    pub query: String,
    pub status: Option<MemoryStatus>,
    project: String,
    entries: Vec<MemoryEntry>,
    error: Option<String>,
}

impl MemoryPane {
    pub fn render(&mut self, ui: &mut egui::Ui, project: Option<&str>) {
        let Some(project) = project else {
            ui.label("Select a project to browse memory.");
            return;
        };
        if self.config.is_none() {
            ui.label("Memory storage is not connected.");
            return;
        }
        let mut refresh = self.project != project;
        self.project = project.into();
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
            ui.colored_label(crate::theme::tokens::ERROR_FG, error);
        }
        ui.label(crate::theme::text::muted(format!(
            "{} lessons (up to 100)",
            self.entries.len()
        )));
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
                        });
                        ui.label(&entry.lesson.content);
                        ui.collapsing("Evidence", |ui| {
                            ui.label(&entry.lesson.evidence);
                            ui.monospace(&entry.lesson.task_id);
                        });
                        ui.separator();
                    });
                }
            });
    }
}
