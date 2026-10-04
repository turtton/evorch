//! Diagnostic history cleanup controls. Database mutations go through the storage writer.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use storage::{DiagnosticCleanupScope, DiagnosticCleanupSummary, StorageConfig, StorageHandle};

use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;
use crate::theme::{
    text::{h3, muted},
    tokens::{PROVIDER_MODAL_MAX_WIDTH, SP_2, SP_4, palette},
    widgets::{primary_button, surface_frame},
};

enum JobResult {
    Preview(Result<DiagnosticCleanupSummary, String>),
    Cleanup(Result<DiagnosticCleanupSummary, String>),
}

#[derive(Default)]
pub(super) struct StorageSettings {
    pub open: bool,
    handle: Option<StorageHandle>,
    db_path: Option<PathBuf>,
    max_db_bytes: u64,
    preview: Option<DiagnosticCleanupSummary>,
    job: Option<Receiver<JobResult>>,
    error: Option<String>,
    last_cleanup: Option<DiagnosticCleanupSummary>,
    confirm_delete: bool,
}

impl StorageSettings {
    pub fn configure(&mut self, handle: StorageHandle, config: &StorageConfig) {
        self.handle = Some(handle);
        self.db_path = Some(config.db_path.clone());
        self.max_db_bytes = config.hard_limits.max_db_bytes;
    }

    fn open(&mut self) {
        self.open = true;
        self.confirm_delete = false;
        self.error = None;
        self.last_cleanup = None;
        self.refresh();
    }

    fn refresh(&mut self) {
        if self.job.is_some() {
            return;
        }
        let Some(handle) = self.handle.clone() else {
            self.error = Some("Storage is not connected".into());
            return;
        };
        let (tx, rx) = mpsc::channel();
        self.job = Some(rx);
        self.preview = None;
        std::thread::spawn(move || {
            let result = handle
                .preview_diagnostic_cleanup(DiagnosticCleanupScope::All)
                .map_err(|error| error.to_string());
            let _ = tx.send(JobResult::Preview(result));
        });
    }

    fn cleanup(&mut self) {
        if self.job.is_some() || !self.confirm_delete {
            return;
        }
        let Some(handle) = self.handle.clone() else {
            self.error = Some("Storage is not connected".into());
            return;
        };
        let (tx, rx) = mpsc::channel();
        self.job = Some(rx);
        self.confirm_delete = false;
        self.error = None;
        self.last_cleanup = None;
        std::thread::spawn(move || {
            let result = handle
                .cleanup_diagnostics(DiagnosticCleanupScope::All)
                .map_err(|error| error.to_string());
            let _ = tx.send(JobResult::Cleanup(result));
        });
    }

    fn poll(&mut self) -> bool {
        let Some(rx) = self.job.take() else {
            return false;
        };
        match rx.try_recv() {
            Ok(JobResult::Preview(Ok(preview))) => {
                self.preview = Some(preview);
                self.error = None;
                true
            }
            Ok(JobResult::Cleanup(Ok(summary))) => {
                self.last_cleanup = Some(summary);
                self.preview = None;
                self.refresh();
                true
            }
            Ok(JobResult::Preview(Err(error)) | JobResult::Cleanup(Err(error))) => {
                self.error = Some(error);
                true
            }
            Err(TryRecvError::Empty) => {
                self.job = Some(rx);
                false
            }
            Err(TryRecvError::Disconnected) => {
                self.error = Some("Storage worker stopped without a result".into());
                true
            }
        }
    }

    fn used_bytes(&self) -> Option<u64> {
        let path = self.db_path.as_ref()?;
        let db = std::fs::metadata(path).ok()?.len();
        let sidecars: u64 = [suffix(path, "-wal"), suffix(path, "-shm")]
            .iter()
            .filter_map(|path| std::fs::metadata(path).ok().map(|meta| meta.len()))
            .sum();
        Some(db.saturating_add(sidecars))
    }
}

fn suffix(path: &Path, extension: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(extension);
    PathBuf::from(value)
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.2} GiB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
    } else {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    }
}

impl<S: AgentRunSource> WorkbenchState<S> {
    pub fn open_storage_settings(&mut self) {
        self.provider_settings.open = false;
        self.routing_settings.open = false;
        self.role_settings.open = false;
        self.sandbox_settings.open = false;
        self.self_improvement_settings.open = false;
        self.close_theme_settings();
        self.storage_settings.open();
    }

    pub(super) fn poll_storage_settings(&mut self) {
        self.storage_settings.poll();
    }

    pub(super) fn render_storage_settings(&mut self, ctx: &egui::Context) {
        if !self.storage_settings.open {
            return;
        }
        let model = &mut self.storage_settings;
        let mut refresh = false;
        let mut delete = false;
        let mut close = false;
        egui::Modal::new(egui::Id::new("storage-settings"))
            .backdrop_color(palette().OVERLAY)
            .frame(surface_frame(palette().SURFACE_RAISED))
            .show(ctx, |ui| {
                ui.set_width(
                    (ctx.viewport_rect().width() * 0.6).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 4.0,
                );
                ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
                ui.label(h3("Storage"));
                if let Some(used) = model.used_bytes() {
                    ui.label(format!(
                        "Event database: {} / {}",
                        format_size(used),
                        format_size(model.max_db_bytes)
                    ));
                }
                ui.label(muted(format!(
                    "Ordinary diagnostic events older than {} days are removed automatically.",
                    storage::DIAGNOSTIC_RETENTION_DAYS
                )));
                ui.label(muted(
                    "Conversation, task, and approval audit records are kept.",
                ));
                if let Some(preview) = &model.preview {
                    ui.label(format!(
                        "Diagnostic history available to delete: {} events ({} of payload)",
                        preview.event_count,
                        format_size(preview.payload_bytes)
                    ));
                    if preview.requires_full_vacuum {
                        ui.label(muted(
                            "This older database needs a separate compaction to reduce file size.",
                        ));
                    }
                    if preview.writes_suspended {
                        ui.colored_label(
                            palette().ERROR_FG,
                            "Storage writes are suspended at the size limit.",
                        );
                    }
                }
                if let Some(cleanup) = &model.last_cleanup {
                    ui.label(format!(
                        "Deleted {} diagnostic events; reclaimed {}.",
                        cleanup.event_count,
                        format_size(cleanup.reclaimed_bytes)
                    ));
                    if cleanup.requires_full_vacuum {
                        ui.label(muted(
                            "This older database needs a full VACUUM to release unused file space.",
                        ));
                    }
                    if cleanup.writes_suspended {
                        ui.colored_label(palette().ERROR_FG, "Storage writes are still suspended.");
                    }
                    if let Some(error) = &cleanup.maintenance_error {
                        ui.colored_label(palette().ERROR_FG, error);
                    }
                }
                if let Some(error) = &model.error {
                    ui.colored_label(palette().ERROR_FG, error);
                }
                if model.job.is_some() {
                    ui.label(muted("Checking or cleaning storage…"));
                }
                ui.horizontal(|ui| {
                    refresh = ui
                        .add_enabled(model.job.is_none(), egui::Button::new("Refresh"))
                        .clicked();
                    if !model.confirm_delete {
                        if ui
                            .add_enabled(
                                model.job.is_none()
                                    && model
                                        .preview
                                        .as_ref()
                                        .is_some_and(|preview| preview.event_count > 0),
                                egui::Button::new("Delete diagnostic history"),
                            )
                            .clicked()
                        {
                            model.confirm_delete = true;
                        }
                    } else {
                        delete = primary_button(ui, "Confirm deletion").clicked();
                        if ui.button("Cancel deletion").clicked() {
                            model.confirm_delete = false;
                        }
                    }
                    close = ui.button("Close").clicked();
                });
            });
        if refresh {
            model.refresh();
        }
        if delete {
            model.cleanup();
        }
        if close {
            model.open = false;
            model.confirm_delete = false;
        }
    }
}
