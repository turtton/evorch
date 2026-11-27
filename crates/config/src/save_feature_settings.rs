//! Edits the Features settings (`pull_requests`, `sandbox.web_tools_enabled`,
//! `tools.browser`, `storage.thread_retention`) inside a configuration text.
//!
//! The edit is a pure text transformation: the caller (the settings area of the
//! core) holds the writer lock, compares the revision, checks the result against
//! the effective configuration and writes it. Only the keys named by the edit are
//! touched; every other key and comment stays, and a key that is absent and
//! already has the wanted effective value is not touched.

use toml_edit::{DocumentMut, Item, Value};

use crate::{
    BrowserToolsConfig, Config, ConfigError, PullRequestsConfig, ThreadRetentionConfig,
    save_agent_features::table_at,
};

/// The sections of the Features screen a save may replace. `None` leaves a section alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeatureSettingsEdit {
    pub pull_requests: Option<PullRequestsConfig>,
    /// `sandbox.web_tools_enabled`.
    pub web_tools_enabled: Option<bool>,
    pub browser: Option<BrowserToolsConfig>,
    pub thread_retention: Option<ThreadRetentionConfig>,
}

/// A TOML scalar the edit writes.
enum Scalar {
    Bool(bool),
    Int(i64),
    Text(String),
}

impl Scalar {
    fn int(value: impl TryInto<i64>) -> Result<Self, ConfigError> {
        value.try_into().map(Self::Int).map_err(|_| {
            crate::save::invalid_field("feature settings", "a number is too large to be saved")
        })
    }

    fn is(&self, value: &Value) -> bool {
        match self {
            Self::Bool(want) => value.as_bool() == Some(*want),
            Self::Int(want) => value.as_integer() == Some(*want),
            Self::Text(want) => value.as_str() == Some(want.as_str()),
        }
    }

    fn value(&self) -> Value {
        match self {
            Self::Bool(value) => Value::from(*value),
            Self::Int(value) => Value::from(*value),
            Self::Text(value) => Value::from(value.as_str()),
        }
    }
}

fn existing<'a>(doc: &'a DocumentMut, tables: &[&str], key: &str) -> Option<&'a Item> {
    let mut table = doc.as_table() as &dyn toml_edit::TableLike;
    for name in tables {
        table = table.get(name)?.as_table_like()?;
    }
    table.get(key)
}

/// Sets `key` to `want`. A key whose effective value is already `want` is not touched, whether
/// or not the text has it (another layer may be what sets it, and the text keeps its own
/// value). Otherwise an existing value is updated in place (its comments stay) and an absent
/// key is created.
fn put(
    doc: &mut DocumentMut,
    tables: &[&str],
    key: &str,
    want: &Scalar,
    already_effective: bool,
) -> Result<(), ConfigError> {
    if already_effective {
        return Ok(());
    }
    let table = table_at(doc, tables)?;
    match table.get_mut(key) {
        Some(Item::Value(value)) => {
            if !want.is(value) {
                let decor = value.decor().clone();
                *value = want.value();
                *value.decor_mut() = decor;
            }
        }
        _ => {
            table.insert(key, Item::Value(want.value()));
        }
    }
    Ok(())
}

/// Removes `key` when it is there (an unset optional value).
fn remove(doc: &mut DocumentMut, tables: &[&str], key: &str) -> Result<(), ConfigError> {
    if existing(doc, tables, key).is_some() {
        table_at(doc, tables)?.remove(key);
    }
    Ok(())
}

/// `text` with the sections of `edit` written. `baseline` is the effective
/// configuration the text is edited from: a key already effective at the wanted
/// value is left as the text has it (or left out).
///
/// # Errors
/// The text is not a configuration document (not TOML, or an unsupported version), a
/// key on the way is not a table, or a number does not fit a TOML integer.
pub fn edit_feature_settings_text(
    text: &str,
    edit: &FeatureSettingsEdit,
    baseline: &Config,
) -> Result<String, ConfigError> {
    let mut doc = crate::save::parse_document(text, "config.toml")?;
    if let Some(pr) = &edit.pull_requests {
        let was = &baseline.pull_requests;
        let table = ["pull_requests"];
        put(
            &mut doc,
            &table,
            "enabled",
            &Scalar::Bool(pr.enabled),
            was.enabled == pr.enabled,
        )?;
        put(
            &mut doc,
            &table,
            "auto_fix_enabled",
            &Scalar::Bool(pr.auto_fix_enabled),
            was.auto_fix_enabled == pr.auto_fix_enabled,
        )?;
        put(
            &mut doc,
            &table,
            "include_in_review",
            &Scalar::Bool(pr.include_in_review),
            was.include_in_review == pr.include_in_review,
        )?;
        for (key, want, was) in [
            (
                "pending_poll_seconds",
                pr.pending_poll_seconds,
                was.pending_poll_seconds,
            ),
            (
                "stable_poll_seconds",
                pr.stable_poll_seconds,
                was.stable_poll_seconds,
            ),
            (
                "review_max_age_seconds",
                pr.review_max_age_seconds,
                was.review_max_age_seconds,
            ),
            (
                "review_refresh_timeout_seconds",
                pr.review_refresh_timeout_seconds,
                was.review_refresh_timeout_seconds,
            ),
            (
                "fetch_timeout_seconds",
                pr.fetch_timeout_seconds,
                was.fetch_timeout_seconds,
            ),
            (
                "backoff_max_seconds",
                pr.backoff_max_seconds,
                was.backoff_max_seconds,
            ),
        ] {
            put(&mut doc, &table, key, &Scalar::int(want)?, want == was)?;
        }
    }
    if let Some(enabled) = edit.web_tools_enabled {
        put(
            &mut doc,
            &["sandbox"],
            "web_tools_enabled",
            &Scalar::Bool(enabled),
            baseline.sandbox.web_tools_enabled == enabled,
        )?;
    }
    if let Some(browser) = &edit.browser {
        let was = &baseline.tools.browser;
        let table = ["tools", "browser"];
        put(
            &mut doc,
            &table,
            "enabled",
            &Scalar::Bool(browser.enabled),
            was.enabled == browser.enabled,
        )?;
        match &browser.executable {
            Some(path) => put(
                &mut doc,
                &table,
                "executable",
                &Scalar::Text(path.to_string_lossy().into_owned()),
                was.executable.as_ref() == Some(path),
            )?,
            None if was.executable.is_some() => remove(&mut doc, &table, "executable")?,
            None => {}
        }
        put(
            &mut doc,
            &table,
            "max_pages",
            &Scalar::int(browser.max_pages.get())?,
            was.max_pages == browser.max_pages,
        )?;
        put(
            &mut doc,
            &table,
            "max_tabs",
            &Scalar::int(browser.max_tabs.get())?,
            was.max_tabs == browser.max_tabs,
        )?;
        put(
            &mut doc,
            &table,
            "coordinate_actions",
            &Scalar::Bool(browser.coordinate_actions),
            was.coordinate_actions == browser.coordinate_actions,
        )?;
    }
    if let Some(retention) = &edit.thread_retention {
        let was = &baseline.storage.thread_retention;
        let table = ["storage", "thread_retention"];
        put(
            &mut doc,
            &table,
            "auto_archive_after_days",
            &Scalar::int(retention.auto_archive_after_days)?,
            was.auto_archive_after_days == retention.auto_archive_after_days,
        )?;
        put(
            &mut doc,
            &table,
            "archived_delete_after_days",
            &Scalar::int(retention.archived_delete_after_days)?,
            was.archived_delete_after_days == retention.archived_delete_after_days,
        )?;
    }
    Ok(doc.to_string())
}

impl FeatureSettingsEdit {
    /// The sections this edit names that `config` does not have as named.
    pub fn differing(&self, config: &Config) -> Vec<&'static str> {
        let mut found = Vec::new();
        if self
            .pull_requests
            .is_some_and(|pr| pr != config.pull_requests)
        {
            found.push("pull_requests");
        }
        if self
            .web_tools_enabled
            .is_some_and(|enabled| enabled != config.sandbox.web_tools_enabled)
        {
            found.push("sandbox.web_tools_enabled");
        }
        if self
            .browser
            .as_ref()
            .is_some_and(|browser| browser != &config.tools.browser)
        {
            found.push("tools.browser");
        }
        if self
            .thread_retention
            .is_some_and(|retention| retention != config.storage.thread_retention)
        {
            found.push("storage.thread_retention");
        }
        found
    }
}
