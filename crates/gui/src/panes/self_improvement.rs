use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use storage::improvement::{ImprovementCandidate, ImprovementStatus};
use storage::{Database, StorageConfig, StorageHandle};

const POLL_INTERVAL_SECS: f64 = 1.0;

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
    resolved_project: Option<(PathBuf, String)>,
    database: Option<Database>,
    database_path: Option<PathBuf>,
    data_version: Option<i64>,
    next_poll_at: f64,
    loaded_status: Option<ImprovementStatus>,
}

impl Default for SelfImprovementPane {
    /// Opens on the untriaged candidates; the other statuses are a filter away.
    fn default() -> Self {
        Self {
            config: None,
            handle: None,
            enabled: false,
            draft_dir: None,
            status: Some(ImprovementStatus::New),
            project: None,
            entries: Vec::new(),
            error: None,
            draft_errors: BTreeMap::new(),
            refresh: false,
            resolved_project: None,
            database: None,
            database_path: None,
            data_version: None,
            next_poll_at: 0.0,
            loaded_status: None,
        }
    }
}

/// Icon and color that stand for a candidate's triage status.
fn status_icon(status: ImprovementStatus) -> (&'static str, egui::Color32) {
    use crate::theme::{icons, tokens::palette};
    match status {
        ImprovementStatus::New => (icons::SPARKLE, palette().ACCENT),
        ImprovementStatus::Reviewed => (icons::CHECK_CIRCLE, palette().SUCCESS),
        ImprovementStatus::Dismissed => (icons::PROHIBIT, palette().TEXT_MUTED),
    }
}

impl SelfImprovementPane {
    /// Uses the same repository identity as the runtime collector, independently
    /// of sidebar display IDs. Resolve Git metadata only when selection changes.
    pub fn render_for_repo_root(&mut self, ui: &mut egui::Ui, root: Option<&Path>) {
        if !self.enabled || self.config.is_none() || root.is_none() {
            self.render(ui, None);
            return;
        }
        let root = root.expect("checked above");
        if self
            .resolved_project
            .as_ref()
            .map(|(path, _)| path.as_path())
            != Some(root)
        {
            self.resolved_project =
                Some((root.to_owned(), crate::runtime_sink::derive_repo_slug(root)));
        }
        let project = self.resolved_project.as_ref().unwrap().1.clone();
        self.render(ui, Some(&project));
    }

    pub fn render(&mut self, ui: &mut egui::Ui, project: Option<&str>) {
        if !self.enabled {
            ui.label("Self-improvement drafts are disabled ([self_improvement] enabled=false). Candidates are not collected.");
            return;
        }
        if self.config.is_none() {
            ui.label("Self-improvement storage is not connected.");
            return;
        }
        let Some(project) = project else {
            ui.label("Select a project to browse improvement candidates.");
            return;
        };
        let path = &self.config.as_ref().unwrap().db_path;
        let storage_changed = self.database_path.as_ref() != Some(path);
        if storage_changed {
            self.database_path = Some(path.clone());
            self.database = None;
            self.data_version = None;
        }
        let mut refresh = storage_changed
            || self.project.as_deref() != Some(project)
            || self.loaded_status != self.status
            || self.refresh;
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
        });
        let now = ui.input(|input| input.time);
        if refresh || now >= self.next_poll_at {
            self.refresh = false;
            self.next_poll_at = now + POLL_INTERVAL_SECS;
            self.loaded_status = self.status;
            let result = (|| {
                if self.database.is_none() {
                    self.database = Some(Database::open(self.config.as_ref().unwrap())?);
                }
                let db = self.database.as_ref().unwrap();
                // data_version is comparable only across reads on one connection.
                // Sample before SELECT so a concurrent write forces the next reload.
                let version = db.pragma_i64("data_version")?;
                if refresh || self.data_version != Some(version) {
                    let entries = db.improvement_candidates(project, self.status, 200)?;
                    self.draft_errors
                        .retain(|id, _| entries.iter().any(|entry| &entry.id == id));
                    self.entries = entries;
                    self.error = None;
                    self.data_version = Some(version);
                }
                Ok::<_, storage::StorageError>(())
            })();
            if let Err(error) = result {
                self.entries.clear();
                self.error = Some(error.to_string());
                self.data_version = None;
            }
        }
        ui.ctx()
            .request_repaint_after(Duration::from_secs_f64((self.next_poll_at - now).max(0.0)));
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
                            let (icon, color) = status_icon(entry.status);
                            let status = entry.status.as_str();
                            let response = ui.add(
                                egui::Label::new(
                                    egui::RichText::new(icon)
                                        .size(crate::theme::tokens::FONT_ICON)
                                        .color(color),
                                )
                                .sense(egui::Sense::hover()),
                            );
                            response.widget_info(|| {
                                egui::WidgetInfo::labeled(egui::WidgetType::Label, true, status)
                            });
                            response.on_hover_text(format!("Status: {status}"));
                            ui.monospace(&entry.id);
                            ui.monospace(format_created_at(entry.created_at_ns));
                        });
                        ui.horizontal_wrapped(|ui| {
                            for badge in
                                [entry.source.as_str(), entry.severity.as_str(), &entry.code]
                            {
                                ui.label(crate::theme::text::badge(badge));
                            }
                        });
                        ui.label(&entry.title);
                        if entry.occurrences > 1 {
                            ui.label(crate::theme::text::muted(format!(
                                "Seen {} times, last {}",
                                entry.occurrences,
                                format_created_at(entry.last_seen_at_ns)
                            )));
                        }
                        ui.collapsing(crate::theme::text::badge("Evidence"), |ui| {
                            ui.monospace(&entry.evidence);
                            if let Some(run_id) = &entry.run_id {
                                ui.monospace(format!("Run: {run_id}"));
                            }
                            if entry.recent_run_ids.len() > 1 {
                                ui.monospace(format!(
                                    "Recent runs: {}",
                                    entry.recent_run_ids.join(", ")
                                ));
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
                    self.refresh = true;
                    self.error =
                        Some("Candidate no longer exists. Reloading automatically.".into());
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
