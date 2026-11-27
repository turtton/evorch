//! The Features sections of the `settings` area: the one save of the PR view,
//! Agent web tools, Agent browser and Thread cleanup, and the view that tells the
//! saved values from the ones the running core uses.
//!
//! * A save edits the text of `config.toml` and goes through the same checks as a
//!   whole-text save (revision, restart targets, strict and effective load) in one
//!   atomic write. Its work runs under the writer lock end to end: write, read the
//!   effective configuration again, apply, announce.
//! * The PR view and Agent web tools follow the effective configuration read after the
//!   write, at once, and only for the sections the save names. A save that failed
//!   changed nothing, and a failure to apply is not a failure of the save: the file
//!   is saved and the view shows the difference.
//! * The browser tools and Thread cleanup are built when the core starts, so their
//!   running values are the startup ones. A saved value that differs from the running
//!   one waits for a core restart, and goes back to not waiting when the value is put
//!   back. Neither depends on `reload_models`.
//! * `applied_revision` is about the models. A save of these keys cannot change what the
//!   models use, so when nothing was waiting for a reload before the save, the new
//!   revision is applied as well; when something was, it stays waiting.
use super::*;
use config::{
    BrowserToolsConfig, Config, FeatureSettingsEdit, PullRequestsConfig, ThreadRetentionConfig,
    edit_feature_settings_text,
};
use std::num::NonZeroU32;
use workbench_protocol::namespaces::settings::{
    BrowserFeature, FeatureSection, FeatureSettingsPatch, FeatureSettingsView, FeatureValues,
    PullRequestFeature, SaveFeatureSettings, SettingsProblem, ThreadCleanupFeature,
};

/// What the running core lets the settings area read and change of the Features it can
/// follow at once. `None` from a getter: the running core has nothing of its own for it,
/// and the values it started with are shown.
pub trait FeatureRuntime: Send + Sync {
    fn pull_requests(&self) -> Option<PullRequestsConfig>;
    /// # Errors
    /// The core has no PR service to apply them to (nothing changed).
    fn apply_pull_requests(&self, settings: PullRequestsConfig) -> Result<(), String>;
    fn web_tools_enabled(&self) -> Option<bool>;
    fn set_web_tools_enabled(&self, enabled: bool);
}

impl FeatureRuntime for runtime::AgentRuntime {
    fn pull_requests(&self) -> Option<PullRequestsConfig> {
        runtime::AgentRuntime::pull_requests(self).map(|service| service.settings())
    }

    fn apply_pull_requests(&self, settings: PullRequestsConfig) -> Result<(), String> {
        let service = runtime::AgentRuntime::pull_requests(self)
            .ok_or_else(|| "pull request tracking has not started in this core".to_owned())?;
        service
            .update_settings(settings)
            .map_err(|error| error.to_string())
    }

    fn web_tools_enabled(&self) -> Option<bool> {
        Some(runtime::AgentRuntime::web_tools_enabled(self))
    }

    fn set_web_tools_enabled(&self, enabled: bool) {
        runtime::AgentRuntime::set_web_tools_enabled(self, enabled);
    }
}

/// A core with nothing running behind the settings area (the unwired form).
pub struct Unwired;

impl FeatureRuntime for Unwired {
    fn pull_requests(&self) -> Option<PullRequestsConfig> {
        None
    }

    fn apply_pull_requests(&self, _: PullRequestsConfig) -> Result<(), String> {
        Err("pull request tracking has not started in this core".into())
    }

    fn web_tools_enabled(&self) -> Option<bool> {
        None
    }

    fn set_web_tools_enabled(&self, _: bool) {}
}

/// What the files held when they were last read.
#[derive(Clone)]
pub(super) struct SavedFeatures {
    revision: SettingsRevision,
    values: Result<FeatureValues, SettingsProblem>,
}

impl SavedFeatures {
    pub(super) fn read(store: &ConfigStore) -> Self {
        match store.effective() {
            Ok((revision, config)) => Self {
                revision,
                values: Ok(values_of(&config)),
            },
            Err(error) => Self {
                revision: store.revision().unwrap_or_else(|_| revision_of(&[])),
                values: Err(match error {
                    SaveError::Io(_) => SettingsProblem::Unreadable,
                    _ => SettingsProblem::InvalidConfig,
                }),
            },
        }
    }
}

fn pull_requests_of(pr: &PullRequestsConfig) -> PullRequestFeature {
    PullRequestFeature {
        enabled: pr.enabled,
        auto_fix_enabled: pr.auto_fix_enabled,
        include_in_review: pr.include_in_review,
        pending_poll_seconds: pr.pending_poll_seconds,
        stable_poll_seconds: pr.stable_poll_seconds,
        timeout_seconds: pr.fetch_timeout_seconds,
    }
}

pub(super) fn values_of(config: &Config) -> FeatureValues {
    let browser = &config.tools.browser;
    let retention = &config.storage.thread_retention;
    FeatureValues {
        pull_requests: pull_requests_of(&config.pull_requests),
        agent_web_tools: config.sandbox.web_tools_enabled,
        browser: BrowserFeature {
            enabled: browser.enabled,
            executable: browser
                .executable
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            max_pages: browser.max_pages.get(),
            max_tabs: browser.max_tabs.get(),
            coordinate_actions: browser.coordinate_actions,
        },
        thread_cleanup: ThreadCleanupFeature {
            auto_archive_after_days: retention.auto_archive_after_days,
            archived_delete_after_days: retention.archived_delete_after_days,
        },
    }
}

fn differing(saved: &FeatureValues, running: &FeatureValues) -> Vec<FeatureSection> {
    let mut sections = Vec::new();
    if saved.pull_requests != running.pull_requests {
        sections.push(FeatureSection::PullRequests);
    }
    if saved.agent_web_tools != running.agent_web_tools {
        sections.push(FeatureSection::AgentWebTools);
    }
    if saved.browser != running.browser {
        sections.push(FeatureSection::Browser);
    }
    if saved.thread_cleanup != running.thread_cleanup {
        sections.push(FeatureSection::ThreadCleanup);
    }
    sections
}

/// The edit a patch asks for, over the effective configuration it starts from: the keys the
/// screen does not show keep the values they have.
fn edit_of(
    patch: &FeatureSettingsPatch,
    baseline: &Config,
) -> Result<FeatureSettingsEdit, SaveError> {
    let tabs = |value: u32, field: &str| {
        NonZeroU32::new(value)
            .ok_or_else(|| SaveError::Invalid(format!("{field} must be 1 or more")))
    };
    Ok(FeatureSettingsEdit {
        pull_requests: patch.pull_requests.map(|pr| PullRequestsConfig {
            enabled: pr.enabled,
            auto_fix_enabled: pr.auto_fix_enabled,
            include_in_review: pr.include_in_review,
            pending_poll_seconds: pr.pending_poll_seconds,
            stable_poll_seconds: pr.stable_poll_seconds,
            fetch_timeout_seconds: pr.timeout_seconds,
            ..baseline.pull_requests
        }),
        web_tools_enabled: patch.agent_web_tools,
        browser: patch
            .browser
            .as_ref()
            .map(|browser| {
                Ok::<_, SaveError>(BrowserToolsConfig {
                    enabled: browser.enabled,
                    executable: browser.executable.as_ref().map(std::path::PathBuf::from),
                    max_pages: tabs(browser.max_pages, "tools.browser.max_pages")?,
                    max_tabs: tabs(browser.max_tabs, "tools.browser.max_tabs")?,
                    coordinate_actions: browser.coordinate_actions,
                })
            })
            .transpose()?,
        thread_retention: patch.thread_cleanup.map(|cleanup| ThreadRetentionConfig {
            auto_archive_after_days: cleanup.auto_archive_after_days,
            archived_delete_after_days: cleanup.archived_delete_after_days,
            ..baseline.storage.thread_retention
        }),
    })
}

impl SettingsArea {
    /// The Features settings: what the files hold, what the running core uses and the
    /// sections waiting for a restart. Answered from what the last read of the files
    /// found; nothing blocks.
    ///
    /// # Errors
    /// Only the unwired form refuses.
    pub fn feature_settings(&self) -> Result<FeatureSettingsView, WorkbenchError> {
        let inner = self.inner()?;
        let saved = lock(&inner.state).features.clone();
        let running = inner.running_features();
        Ok(match saved.values {
            Ok(values) => FeatureSettingsView {
                revision: saved.revision,
                restart_pending: differing(&values, &running),
                saved: values,
                running,
                problem: None,
            },
            Err(problem) => FeatureSettingsView {
                revision: saved.revision,
                saved: running.clone(),
                running,
                restart_pending: Vec::new(),
                problem: Some(problem),
            },
        })
    }
}

impl Inner {
    /// The values the running core uses.
    fn running_features(&self) -> FeatureValues {
        let mut values = values_of(self.store.startup());
        if let Some(pr) = self.features.pull_requests() {
            values.pull_requests = pull_requests_of(&pr);
        }
        if let Some(enabled) = self.features.web_tools_enabled() {
            values.agent_web_tools = enabled;
        }
        values
    }

    pub(super) async fn save_feature_settings(
        &self,
        save: SaveFeatureSettings,
    ) -> Result<Done, Failure> {
        // Held to the end: the write, the effective configuration read after it, what is
        // applied from that and the announcement are one step for every other save.
        let _writer = self.writer.clone().lock_owned().await;
        let applied_before = lock(&self.state).snapshot.applied_revision.clone();
        let SaveFeatureSettings {
            expected_revision,
            changes,
            ..
        } = save;
        let sections = changes.sections();
        let store = self.store.clone();
        let quoted = expected_revision.clone();
        let (revision, effective) = Self::blocking(move || {
            let revision = store.save_edited(
                &quoted,
                |text, baseline| {
                    let edit = edit_of(&changes, baseline)?;
                    edit_feature_settings_text(text, &edit, baseline).map_err(SaveError::from)
                },
                |effective| {
                    let edit = edit_of(&changes, effective)?;
                    let overridden = edit.differing(effective);
                    if overridden.is_empty() {
                        Ok(())
                    } else {
                        Err(SaveError::Invalid(format!(
                            "{} would not take effect: another configuration layer overrides it",
                            overridden.join(", ")
                        )))
                    }
                },
            )?;
            Ok::<_, SaveError>((revision, store.effective_strict()))
        })
        .await?
        .map_err(Failure::from)?;
        let message = self.apply_features(&sections, effective);
        self.reread().await?;
        if applied_before.as_ref() == Some(&expected_revision) {
            self.set_applied(Some(revision.clone()), None);
        }
        Ok(Done {
            revision: Some(revision),
            message,
        })
    }

    /// Follow the saved values with the sections that can be followed at once. `None`: all
    /// that was named was applied. Otherwise the sentence that says the file is saved and
    /// the running core is not following it.
    fn apply_features(
        &self,
        sections: &[FeatureSection],
        effective: Result<Config, SaveError>,
    ) -> Option<String> {
        let live = [FeatureSection::PullRequests, FeatureSection::AgentWebTools];
        if !sections.iter().any(|section| live.contains(section)) {
            return None;
        }
        let config = match effective {
            Ok(config) => config,
            Err(_) => {
                return Some(
                    "Saved, but the running core could not read the saved settings back to apply them."
                        .into(),
                );
            }
        };
        let mut failed = Vec::new();
        if sections.contains(&FeatureSection::PullRequests)
            && let Err(reason) = self.features.apply_pull_requests(config.pull_requests)
        {
            failed.push(format!("PR view: {}", sanitize(&reason)));
        }
        if sections.contains(&FeatureSection::AgentWebTools) {
            self.features
                .set_web_tools_enabled(config.sandbox.web_tools_enabled);
        }
        (!failed.is_empty()).then(|| {
            format!(
                "Saved, but not applied to the running core ({}). A core restart applies it.",
                failed.join("; ")
            )
        })
    }
}
