use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use storage::improvement::{ImprovementCandidate, ImprovementStatus};
use storage::{Database, StorageConfig, StorageHandle};

#[derive(Default)]
pub struct SelfImprovementPane {
    pub config: Option<StorageConfig>,
    pub handle: Option<StorageHandle>,
    /// Startup enablement, not an unsaved settings draft. Changes require restart.
    pub enabled: bool,
    /// Resolved by the app using `model::self_improvement_settings::resolve_draft_dir`,
    /// exactly as startup resolves the runtime policy. The pane never creates it.
    pub draft_dir: Option<PathBuf>,
    pub status: Option<ImprovementStatus>,
    project: Option<String>,
    entries: Vec<ImprovementCandidate>,
    error: Option<String>,
    draft_errors: BTreeMap<String, String>,
    refresh: bool,
}

impl SelfImprovementPane {
    pub fn render(&mut self, ui: &mut egui::Ui, project: Option<&str>) {
        if !self.enabled {
            ui.label("Self-improvement drafts are disabled ([self_improvement] enabled=false). Candidates are not collected.");
            return;
        }
        let Some(project) = project else {
            ui.label("Select a project to browse improvement candidates.");
            return;
        };
        if self.config.is_none() {
            ui.label("Self-improvement storage is not connected.");
            return;
        }
        let mut refresh = self.project.as_deref() != Some(project) || self.refresh;
        self.project = Some(project.into());
        ui.horizontal_wrapped(|ui| {
            ui.label("Status");
            let status_response = egui::ComboBox::from_id_salt("self_improvement_status")
                .selected_text(self.status.map_or("All", ImprovementStatus::as_str))
                .show_ui(ui, |ui| {
                    refresh |= ui.selectable_value(&mut self.status, None, "All").changed();
                    for status in [
                        ImprovementStatus::New,
                        ImprovementStatus::Reviewed,
                        ImprovementStatus::Dismissed,
                    ] {
                        refresh |= ui
                            .selectable_value(&mut self.status, Some(status), status.as_str())
                            .changed();
                    }
                });
            status_response.response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, true, "Candidate status")
            });
            refresh |= ui.button("Refresh").clicked();
        });
        if refresh {
            self.refresh = false;
            self.draft_errors.clear();
            let result = self.config.as_ref().map(|config| {
                Database::open(config)
                    .and_then(|db| db.improvement_candidates(project, self.status, 200))
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
            "{} candidates (up to 200)",
            self.entries.len()
        )));
        let mut action = None;
        egui::ScrollArea::vertical()
            .id_salt("self_improvement_results")
            .show(ui, |ui| {
                if self.entries.is_empty() && self.error.is_none() {
                    ui.label("No improvement candidates yet.");
                }
                for entry in &self.entries {
                    ui.push_id(&entry.id, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.monospace(&entry.id);
                            ui.monospace(format_created_at(entry.created_at_ns));
                            ui.label(entry.status.as_str());
                        });
                        ui.horizontal_wrapped(|ui| {
                            for badge in
                                [entry.source.as_str(), entry.severity.as_str(), &entry.code]
                            {
                                ui.label(crate::theme::text::badge(badge));
                            }
                        });
                        ui.label(&entry.title);
                        ui.collapsing(crate::theme::text::badge("Evidence"), |ui| {
                            ui.monospace(&entry.evidence);
                            if let Some(run_id) = &entry.run_id {
                                ui.monospace(format!("Run: {run_id}"));
                            }
                        });
                        if let Some(path) = &entry.draft_path {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(format!("Draft: {path}"));
                                if ui.button("Copy issue draft").clicked() {
                                    let result = self
                                        .draft_dir
                                        .as_deref()
                                        .ok_or_else(|| {
                                            "Draft directory is not configured".to_owned()
                                        })
                                        .and_then(|root| {
                                            read_draft(root, path)
                                                .map_err(|error| error.to_string())
                                        });
                                    match result {
                                        Ok(text) => {
                                            ui.output_mut(|output| {
                                                output
                                                    .commands
                                                    .push(egui::OutputCommand::CopyText(text))
                                            });
                                            self.draft_errors.remove(&entry.id);
                                        }
                                        Err(error) => {
                                            self.draft_errors.insert(
                                                entry.id.clone(),
                                                format!("Could not read draft: {error}"),
                                            );
                                        }
                                    }
                                }
                            });
                            if let Some(error) = self.draft_errors.get(&entry.id) {
                                ui.label(crate::theme::text::muted(error));
                            }
                        }
                        if self.handle.is_some() {
                            ui.horizontal(|ui| {
                                if ui.button("Mark reviewed").clicked() {
                                    action = Some((entry.id.clone(), ImprovementStatus::Reviewed));
                                }
                                if ui.button("Dismiss").clicked() {
                                    action = Some((entry.id.clone(), ImprovementStatus::Dismissed));
                                }
                            });
                        }
                        ui.separator();
                    });
                }
            });
        if let Some((id, status)) = action
            && let Some(handle) = &self.handle
        {
            match handle.set_improvement_status(&id, status) {
                Ok(true) => self.refresh = true,
                Ok(false) => {
                    self.error = Some("Candidate no longer exists. Refresh to reload.".into())
                }
                Err(error) => self.error = Some(error.to_string()),
            }
            ui.ctx().request_repaint();
        }
    }
}

/// Draft metadata is not a license to read arbitrary files (including via symlinks).
fn read_draft(root: &Path, relative: &str) -> std::io::Result<String> {
    if Path::new(relative)
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(std::io::Error::other(
            "Draft path must be relative to the drafts directory",
        ));
    }
    let root = root.canonicalize()?;
    let path = root.join(relative).canonicalize()?;
    if !path.starts_with(&root) {
        return Err(std::io::Error::other(
            "Draft path escapes the drafts directory",
        ));
    }
    std::fs::read_to_string(path)
}

/// UTC, second precision, RFC3339-style. Storage timestamps fit signed i64
/// nanoseconds (1970–2262); reject out-of-range metadata before conversion.
fn format_created_at(ns: u64) -> String {
    let Ok(ns) = i64::try_from(ns) else {
        return "Unknown creation time".into();
    };
    let seconds = storage::ns_to_system_time(ns)
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut days = seconds / 86_400;
    let mut year = 1970_u64;
    let leap = |year: u64| {
        year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
    };
    loop {
        let length = if leap(year) { 366 } else { 365 };
        if days < length {
            break;
        }
        days -= length;
        year += 1;
    }
    let mut month = 1;
    for length in [
        31,
        if leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ] {
        if days < length {
            break;
        }
        days -= length;
        month += 1;
    }
    format!(
        "{year:04}-{month:02}-{:02}T{:02}:{:02}:{:02}Z",
        days + 1,
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_use_utc_and_handle_leap_days() {
        assert_eq!(format_created_at(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            format_created_at(951_827_696_000_000_000),
            "2000-02-29T12:34:56Z"
        );
        assert_eq!(
            format_created_at(4_107_542_400_000_000_000),
            "2100-03-01T00:00:00Z"
        );
    }

    #[test]
    fn draft_reads_stay_inside_the_resolved_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("drafts");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("candidate.md"), "draft body").unwrap();
        std::fs::write(dir.path().join("outside.md"), "not a draft").unwrap();
        assert_eq!(read_draft(&root, "candidate.md").unwrap(), "draft body");
        assert!(read_draft(&root, "missing.md").is_err());
        assert!(read_draft(&root, "../outside.md").is_err());
        assert!(read_draft(&root, dir.path().join("outside.md").to_str().unwrap()).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("outside.md"), root.join("link.md"))
                .unwrap();
            assert!(read_draft(&root, "link.md").is_err());
        }
    }
}
