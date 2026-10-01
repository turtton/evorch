//! Persisted settings and draft-path resolution, independent of egui.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use config::SelfImprovementConfig;

/// Shared by startup and the pane: an explicit path is used verbatim, otherwise
/// drafts live beside the storage database. Resolution does not create anything.
pub fn resolve_draft_dir(cfg: &SelfImprovementConfig, storage_db_path: &Path) -> PathBuf {
    cfg.draft_dir.as_ref().map_or_else(
        || {
            storage_db_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("self-improvement/drafts")
        },
        PathBuf::from,
    )
}

#[derive(Debug, Default)]
pub struct SelfImprovementSettingsModel {
    pub open: bool,
    pub draft: SelfImprovementConfig,
    pub draft_dir: String,
    pub error: Option<String>,
    save_rx: Option<Receiver<Result<(), String>>>,
}

impl SelfImprovementSettingsModel {
    pub fn seed_from_config(config: &SelfImprovementConfig) -> Self {
        Self {
            draft: config.clone(),
            draft_dir: config.draft_dir.clone().unwrap_or_default(),
            ..Self::default()
        }
    }

    pub const fn is_saving(&self) -> bool {
        self.save_rx.is_some()
    }

    pub fn start_save(&mut self, path: Option<PathBuf>) {
        if self.is_saving() {
            return;
        }
        let Some(path) = path else {
            self.error = Some("No project config path is configured".into());
            return;
        };
        self.draft.draft_dir = if self.draft_dir.trim().is_empty() {
            None
        } else {
            Some(self.draft_dir.clone())
        };
        let draft = self.draft.clone();
        let (tx, rx) = mpsc::channel();
        self.save_rx = Some(rx);
        self.error = None;
        std::thread::spawn(move || {
            let result = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .map_err(|error| error.to_string())
                .and_then(|()| {
                    config::save_self_improvement(&path, &draft).map_err(|error| error.to_string())
                });
            let _ = tx.send(result);
        });
    }

    /// Returns true only when a save completed successfully. Startup configuration
    /// remains unchanged until restart; no collector is started by saving settings.
    pub fn poll_save(&mut self) -> bool {
        let Some(rx) = self.save_rx.take() else {
            return false;
        };
        match rx.try_recv() {
            Ok(Ok(())) => {
                self.error = None;
                true
            }
            Ok(Err(error)) => {
                self.error = Some(error);
                false
            }
            Err(TryRecvError::Empty) => {
                self.save_rx = Some(rx);
                false
            }
            Err(TryRecvError::Disconnected) => {
                self.error =
                    Some("Self-improvement settings worker stopped without a result".into());
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_directory_resolution_uses_override_or_database_parent() {
        let mut cfg = SelfImprovementConfig::default();
        assert_eq!(
            resolve_draft_dir(&cfg, Path::new("state/store.db")),
            Path::new("state/self-improvement/drafts")
        );
        assert_eq!(
            resolve_draft_dir(&cfg, Path::new("store.db")),
            Path::new("self-improvement/drafts")
        );
        cfg.draft_dir = Some("custom/drafts".into());
        assert_eq!(
            resolve_draft_dir(&cfg, Path::new("state/store.db")),
            Path::new("custom/drafts")
        );
    }

    fn wait_for_save(model: &mut SelfImprovementSettingsModel) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while model.is_saving() {
            if model.poll_save() {
                return true;
            }
            assert!(std::time::Instant::now() < deadline, "save timed out");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn async_save_round_trips_all_fields_and_preserves_other_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = config::project_main_config_path(dir.path());
        std::fs::create_dir_all(dir.path().join(config::PROJECT_CONFIG_DIR))
            .expect("config directory");
        std::fs::write(&path, "version = 2\n[metrics]\nenabled = false\n").unwrap();
        let cfg = SelfImprovementConfig {
            enabled: true,
            draft_dir: Some("drafts".into()),
            max_candidates: 42,
            evidence_max_bytes: 512,
            daily_limit: 7,
            duplicate_cooldown_secs: 90,
            collect_diagnostics: false,
            collect_lessons: false,
        };
        let mut model = SelfImprovementSettingsModel::seed_from_config(&cfg);
        model.start_save(Some(path));
        assert!(wait_for_save(&mut model));
        let loaded = config::Config::load(&config::LoadOptions {
            project_dir: Some(dir.path().into()),
            user_config_dir: Some(dir.path().join("user")),
            read_env: false,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(loaded.self_improvement, cfg);
        assert!(!loaded.metrics.enabled);
    }

    #[test]
    fn default_is_disabled_and_failed_save_surfaces_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut model = SelfImprovementSettingsModel::default();
        assert!(!model.draft.enabled);
        model.start_save(None);
        assert!(model.error.is_some());
        model.start_save(Some(dir.path().to_owned()));
        assert!(!wait_for_save(&mut model));
        assert!(model.error.is_some());
    }
}
