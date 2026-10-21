use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use storage::improvement::{ImprovementCandidate, ImprovementStatus};
use storage::{Database, StorageConfig, StorageHandle};

const POLL_INTERVAL_SECS: f64 = 1.0;

mod triage;
pub(crate) use triage::read_directory_error;
pub use triage::{TRIAGE_MAX_ROWS, triage_prompt};

/// Label of the button that opens a draft triage thread (ADR 0037).
pub const TRIAGE_BUTTON: &str = "Create triage thread (reads drafts)";

/// A request the workbench carries out on the pane's behalf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelfImprovementAction {
    /// Open a thread whose runs may read `read_dirs`, never write them, with
    /// `prompt` waiting in its composer for the operator to send.
    CreateTriageThread {
        title: String,
        read_dirs: Vec<PathBuf>,
        prompt: String,
    },
}

pub struct SelfImprovementPane {
    pub config: Option<StorageConfig>,
    pub handle: Option<StorageHandle>,
    /// Startup enablement, not an unsaved settings draft. Changes require restart.
    pub enabled: bool,
    /// Resolved by the app using `model::self_improvement_settings::resolve_draft_dir`,
    /// exactly as startup resolves the runtime policy. The pane never creates it.
    pub draft_dir: Option<PathBuf>,
    /// Active project root: RW project paths cannot be promised as read-only.
    pub project_root: Option<PathBuf>,
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
            project_root: None,
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
    pub fn render_for_repo_root(
        &mut self,
        ui: &mut egui::Ui,
        root: Option<&Path>,
    ) -> Option<SelfImprovementAction> {
        self.project_root = root.map(Path::to_path_buf);
        if !self.enabled || self.config.is_none() || root.is_none() {
            return self.render(ui, None);
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
        self.render(ui, Some(&project))
    }

    pub fn render(
        &mut self,
        ui: &mut egui::Ui,
        project: Option<&str>,
    ) -> Option<SelfImprovementAction> {
        if !self.enabled {
            ui.label("Self-improvement drafts are disabled ([self_improvement] enabled=false). Candidates are not collected.");
            return None;
        }
        if self.config.is_none() {
            ui.label("Self-improvement storage is not connected.");
            return None;
        }
        let Some(project) = project else {
            ui.label("Select a project to browse improvement candidates.");
            return None;
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
        // Checked before this frame's reload; a click is checked again after it.
        let triage = self.triage_target();
        let mut triage_clicked = false;
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
            let button = ui.add_enabled(triage.is_ok(), egui::Button::new(TRIAGE_BUTTON));
            triage_clicked = match &triage {
                Ok(dir) => button
                    .on_hover_text(format!(
                        "Opens a thread whose runs can read {} but not write it, with a triage prompt to review before sending",
                        dir.display()
                    ))
                    .clicked(),
                Err(reason) => {
                    button.on_disabled_hover_text(reason.as_str());
                    false
                }
            };
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
        let triage_action = if triage_clicked {
            match self.triage_target() {
                Ok(dir) => Some(self.triage_action(dir)),
                Err(reason) => {
                    self.error = Some(reason);
                    None
                }
            }
        } else {
            None
        };
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
        let wall_clock = std::time::SystemTime::now();
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
                            if entry.reopened_count > 0 {
                                ui.label(
                                    crate::theme::text::badge(format!(
                                        "Reopened ×{}",
                                        entry.reopened_count
                                    ))
                                    .color(crate::theme::tokens::palette().WARNING_FG)
                                    .background_color(
                                        crate::theme::tokens::palette().WARNING_SURFACE,
                                    ),
                                );
                            }
                            ui.monospace(&entry.id);
                            let (created, absolute) = timestamp(entry.created_at_ns, wall_clock);
                            ui.label(crate::theme::text::muted(created))
                                .on_hover_text(absolute);
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
                            let (seen, absolute) = timestamp(entry.last_seen_at_ns, wall_clock);
                            ui.label(crate::theme::text::muted(format!(
                                "Seen {} times, last {seen}",
                                entry.occurrences
                            )))
                            .on_hover_text(absolute);
                        }
                        ui.collapsing(crate::theme::text::badge("Evidence"), |ui| {
                            ui.monospace(&entry.evidence);
                            if let Some(run_id) = &entry.run_id {
                                ui.horizontal(|ui| {
                                    ui.label(crate::theme::text::muted("Run"));
                                    crate::theme::widgets::copy_id_button(
                                        ui,
                                        "Copy run ID",
                                        run_id,
                                    );
                                });
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
        triage_action
    }

    /// The canonical draft directory a triage thread would read, or why none may.
    fn triage_target(&self) -> Result<PathBuf, String> {
        let Some(dir) = self.draft_dir.as_deref() else {
            return Err("Draft directory is not configured".into());
        };
        let Some(dir) = dir.canonicalize().ok().filter(|path| path.is_dir()) else {
            return Err(format!("Draft directory does not exist: {}", dir.display()));
        };
        if let Some(reason) = read_directory_error(
            &dir,
            self.project_root.as_deref(),
            self.config.as_ref().map(|config| config.db_path.as_path()),
        ) {
            return Err(reason);
        }
        if self.entries.is_empty() {
            return Err("No candidates match the status filter".into());
        }
        Ok(dir)
    }

    fn triage_action(&self, dir: PathBuf) -> SelfImprovementAction {
        let count = self
            .entries
            .iter()
            .filter(|entry| self.status.is_none_or(|status| entry.status == status))
            .count();
        let status = self.status.map_or("all", ImprovementStatus::as_str);
        SelfImprovementAction::CreateTriageThread {
            title: format!("Triage drafts ({count} candidates, {status})"),
            prompt: triage_prompt(&dir, &self.entries, self.status, TRIAGE_MAX_ROWS),
            read_dirs: vec![dir],
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

/// Relative age for the label and absolute UTC time for its tooltip. Storage
/// timestamps fit signed i64 nanoseconds (1970–2262); reject out-of-range
/// metadata before conversion.
fn timestamp(ns: u64, now: std::time::SystemTime) -> (String, String) {
    let Ok(ns) = i64::try_from(ns) else {
        return ("unknown time".into(), "Unknown creation time".into());
    };
    let at = storage::ns_to_system_time(ns);
    (
        crate::format::time_since(at, now),
        crate::format::utc_timestamp(at.into()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_read_as_relative_age_with_absolute_utc_tooltip() {
        let now = std::time::UNIX_EPOCH + Duration::from_secs(951_827_696 + 3_742);
        assert_eq!(
            timestamp(951_827_696_000_000_000, now),
            ("1h 2m ago".into(), "2000-02-29 12:34 UTC".into())
        );
        assert_eq!(timestamp(u64::MAX, now).1, "Unknown creation time");
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
