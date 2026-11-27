//! Features: the settings of the four features the Features screen owns (PR
//! view, Agent web tools, Agent browser, Thread cleanup), saved together and
//! read back with what the running core uses.
//!
//! * `save_feature_settings` is one command for all four sections. The sections
//!   named in the patch are written in one atomic write of `config.toml`, with
//!   the checks every settings save makes (revision, `core.restart.targets`,
//!   strict and effective load). An empty patch is refused.
//! * `get_feature_settings` answers with the saved values, the values the
//!   running core uses and the sections where they differ. A section whose
//!   saved value is not the running one is waiting for a core restart; the PR
//!   view and Agent web tools follow a save at once, so they only wait when the
//!   core could not apply them.
//! * Nothing here changes `applied_revision` or `apply_failure`: those describe
//!   the models (`reload_models`), not these sections.
use super::{SettingsProblem, SettingsRevision};
use crate::{
    ids::OperationId,
    validation::{ValidationError, check_line},
};
use serde::{Deserialize, Serialize};

const MAX_EXECUTABLE_BYTES: usize = 2048;
/// Longest polling or timeout interval a client may ask for, in seconds (one day).
pub const MAX_INTERVAL_SECONDS: u64 = 86_400;
/// Most days a retention value may hold (a hundred years).
pub const MAX_RETENTION_DAYS: u32 = 36_500;
/// Most browser tabs a client may ask for.
pub const MAX_BROWSER_TABS: u32 = 1024;
/// Sections a Features view can list (there are four).
pub const MAX_FEATURE_SECTIONS: usize = 4;

fn invalid(field: &'static str, reason: &'static str) -> ValidationError {
    ValidationError::OutOfRange { field, reason }
}

/// The PR view settings the screen edits. The other `[pull_requests]` keys are
/// kept as they are in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PullRequestFeature {
    pub enabled: bool,
    pub auto_fix_enabled: bool,
    pub include_in_review: bool,
    pub pending_poll_seconds: u64,
    pub stable_poll_seconds: u64,
    /// `pull_requests.fetch_timeout_seconds`.
    pub timeout_seconds: u64,
}

/// The agent browser settings (`[tools.browser]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserFeature {
    pub enabled: bool,
    /// The browser to launch; `None` uses `CHROME`, then auto-detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    pub max_pages: u32,
    pub max_tabs: u32,
    pub coordinate_actions: bool,
}

/// Thread cleanup (`[storage.thread_retention]`). `0` turns a step off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadCleanupFeature {
    pub auto_archive_after_days: u32,
    pub archived_delete_after_days: u32,
}

/// All four sections, as saved or as the running core has them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureValues {
    pub pull_requests: PullRequestFeature,
    pub agent_web_tools: bool,
    pub browser: BrowserFeature,
    pub thread_cleanup: ThreadCleanupFeature,
}

/// One of the four sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureSection {
    PullRequests,
    AgentWebTools,
    Browser,
    ThreadCleanup,
}

/// The sections a save replaces; `None` leaves a section as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureSettingsPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_requests: Option<PullRequestFeature>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_web_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<BrowserFeature>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_cleanup: Option<ThreadCleanupFeature>,
}

impl FeatureSettingsPatch {
    pub fn is_empty(&self) -> bool {
        self.pull_requests.is_none()
            && self.agent_web_tools.is_none()
            && self.browser.is_none()
            && self.thread_cleanup.is_none()
    }

    /// The sections the patch replaces.
    pub fn sections(&self) -> Vec<FeatureSection> {
        let mut sections = Vec::new();
        if self.pull_requests.is_some() {
            sections.push(FeatureSection::PullRequests);
        }
        if self.agent_web_tools.is_some() {
            sections.push(FeatureSection::AgentWebTools);
        }
        if self.browser.is_some() {
            sections.push(FeatureSection::Browser);
        }
        if self.thread_cleanup.is_some() {
            sections.push(FeatureSection::ThreadCleanup);
        }
        sections
    }

    /// Wire bounds only; the ranges and the relations between values are the
    /// configuration's, checked by the core when it saves.
    ///
    /// # Errors
    /// An empty patch, or a value outside the wire bounds.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.is_empty() {
            return Err(invalid("changes", "names no section to save"));
        }
        if let Some(pr) = &self.pull_requests {
            for seconds in [
                pr.pending_poll_seconds,
                pr.stable_poll_seconds,
                pr.timeout_seconds,
            ] {
                if seconds > MAX_INTERVAL_SECONDS {
                    return Err(invalid("pull_requests", "an interval is too long"));
                }
            }
        }
        if let Some(browser) = &self.browser {
            if let Some(executable) = &browser.executable {
                check_line(
                    "browser.executable",
                    executable,
                    MAX_EXECUTABLE_BYTES,
                    false,
                )?;
            }
            for tabs in [browser.max_pages, browser.max_tabs] {
                if !(1..=MAX_BROWSER_TABS).contains(&tabs) {
                    return Err(invalid("browser", "tab limits are out of range"));
                }
            }
        }
        if let Some(cleanup) = &self.thread_cleanup
            && (cleanup.auto_archive_after_days > MAX_RETENTION_DAYS
                || cleanup.archived_delete_after_days > MAX_RETENTION_DAYS)
        {
            return Err(invalid("thread_cleanup", "a number of days is too large"));
        }
        Ok(())
    }
}

/// Save the sections of `changes` in one write of `config.toml`. The file must
/// still be `expected_revision`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveFeatureSettings {
    pub operation_id: OperationId,
    pub expected_revision: SettingsRevision,
    pub changes: FeatureSettingsPatch,
}

/// What the Features screen reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureSettingsView {
    /// The revision of the file `saved` was read from; a save quotes it.
    pub revision: SettingsRevision,
    /// What the files hold.
    pub saved: FeatureValues,
    /// What the running core uses: the PR view and Agent web tools as they are
    /// set now, the browser and thread cleanup as the core started with them.
    pub running: FeatureValues,
    /// The sections where `saved` and `running` differ: they take effect after
    /// a core restart.
    pub restart_pending: Vec<FeatureSection>,
    /// Why `saved` is not read from the file (it then repeats `running`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<SettingsProblem>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn browser() -> BrowserFeature {
        BrowserFeature {
            enabled: true,
            executable: None,
            max_pages: 8,
            max_tabs: 4,
            coordinate_actions: false,
        }
    }

    #[test]
    fn an_empty_patch_and_out_of_range_numbers_are_refused() {
        assert!(FeatureSettingsPatch::default().validate().is_err());
        let patch = |browser: BrowserFeature| FeatureSettingsPatch {
            browser: Some(browser),
            ..Default::default()
        };
        assert!(patch(browser()).validate().is_ok());
        for bad in [
            BrowserFeature {
                max_tabs: 0,
                ..browser()
            },
            BrowserFeature {
                max_pages: MAX_BROWSER_TABS + 1,
                ..browser()
            },
            BrowserFeature {
                executable: Some("two\nlines".into()),
                ..browser()
            },
        ] {
            assert!(patch(bad).validate().is_err());
        }
        assert!(
            FeatureSettingsPatch {
                thread_cleanup: Some(ThreadCleanupFeature {
                    auto_archive_after_days: MAX_RETENTION_DAYS + 1,
                    archived_delete_after_days: 0,
                }),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn a_patch_names_the_sections_it_replaces_and_omits_the_others_on_the_wire() {
        let patch = FeatureSettingsPatch {
            agent_web_tools: Some(false),
            browser: Some(browser()),
            ..Default::default()
        };
        assert_eq!(
            patch.sections(),
            [FeatureSection::AgentWebTools, FeatureSection::Browser]
        );
        let wire = serde_json::to_value(&patch).unwrap();
        assert!(wire.get("pull_requests").is_none() && wire.get("thread_cleanup").is_none());
        assert_eq!(
            serde_json::from_value::<FeatureSettingsPatch>(wire).unwrap(),
            patch
        );
    }
}
