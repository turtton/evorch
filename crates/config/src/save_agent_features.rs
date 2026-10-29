//! Writes the agent feature switches (`sandbox.web_tools_enabled`,
//! `tools.run_js.enabled`) without touching their neighbouring keys.

use std::path::Path;

use toml_edit::{DocumentMut, Item, Table};

use crate::{Config, ConfigError};

/// The on/off switches of agent tool features that the settings UI edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentFeatures {
    /// `sandbox.web_tools_enabled`.
    pub web_tools_enabled: bool,
    /// `tools.run_js.enabled` (ADR 0039).
    pub run_js_enabled: bool,
}

impl AgentFeatures {
    /// The switches as `config` has them.
    pub const fn from_config(config: &Config) -> Self {
        Self {
            web_tools_enabled: config.sandbox.web_tools_enabled,
            run_js_enabled: config.tools.run_js.enabled,
        }
    }
}

impl Default for AgentFeatures {
    fn default() -> Self {
        Self::from_config(&Config::default())
    }
}

/// Sets only the agent feature switches in `path`, retaining every other key
/// (including the other `[sandbox]` and `[tools.run_js]` keys) and comments.
///
/// # Errors
/// Returns configuration validation or filesystem errors, and refuses to write
/// when the saved switches would not take effect: `[tools]` is read only from
/// user files, and a drop-in or other later layer may override the file.
pub fn save_agent_features(path: &Path, features: AgentFeatures) -> Result<(), ConfigError> {
    save_agent_features_in_scope(path, features, crate::save_context::SaveScope::Standalone)
}

pub(crate) fn save_agent_features_in_scope(
    path: &Path,
    features: AgentFeatures,
    scope: crate::save_context::SaveScope<'_>,
) -> Result<(), ConfigError> {
    let mut doc = crate::save::read_document(path)?;
    set_key(
        &mut doc,
        &["sandbox"],
        "web_tools_enabled",
        features.web_tools_enabled,
    )?;
    set_key(
        &mut doc,
        &["tools", "run_js"],
        "enabled",
        features.run_js_enabled,
    )?;
    let checked = crate::save::checked_text_for_save(path, &doc, scope)?;
    let effective = AgentFeatures::from_config(checked.config());
    if effective.web_tools_enabled != features.web_tools_enabled {
        return Err(not_effective(
            "sandbox.web_tools_enabled",
            "another configuration layer overrides it",
        ));
    }
    if effective.run_js_enabled != features.run_js_enabled {
        return Err(not_effective(
            "tools.run_js.enabled",
            "[tools] is read only from the user config, and a later user file must not override it",
        ));
    }
    checked.write()
}

fn not_effective(path: &str, reason: &str) -> ConfigError {
    crate::save::invalid_field(path, &format!("the saved value would not apply: {reason}"))
}

/// Sets `key` in the table at `tables`, creating missing tables. An existing
/// value is updated in place so the comments around it stay.
fn set_key(
    doc: &mut DocumentMut,
    tables: &[&str],
    key: &str,
    enabled: bool,
) -> Result<(), ConfigError> {
    let mut table = doc.as_table_mut() as &mut dyn toml_edit::TableLike;
    let mut path = String::new();
    for (depth, name) in tables.iter().enumerate() {
        if depth > 0 {
            path.push('.');
        }
        path.push_str(name);
        // Intermediate tables stay implicit so no empty `[tools]` header appears.
        let implicit = depth + 1 < tables.len();
        let item = table.entry(name).or_insert_with(|| {
            let mut created = Table::new();
            created.set_implicit(implicit);
            Item::Table(created)
        });
        table = item
            .as_table_like_mut()
            .ok_or_else(|| crate::save::invalid_field(&path, "must be a table"))?;
    }
    match table.get_mut(key) {
        Some(Item::Value(value)) => {
            if value.as_bool() != Some(enabled) {
                let decor = value.decor().clone();
                *value = toml_edit::Value::from(enabled);
                *value.decor_mut() = decor;
            }
        }
        _ => {
            table.insert(key, toml_edit::value(enabled));
        }
    }
    Ok(())
}
