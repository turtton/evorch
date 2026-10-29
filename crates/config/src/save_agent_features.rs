//! Writes the agent feature switches (`sandbox.web_tools_enabled`,
//! `tools.run_js.enabled`) without touching their neighbouring keys.

use std::path::Path;

use toml_edit::{DocumentMut, Item, RawString, Table, Value};

use crate::{Config, ConfigError};

/// The settings of agent tool features that the settings UI edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFeatures {
    /// `sandbox.web_tools_enabled`.
    pub web_tools_enabled: bool,
    /// `tools.run_js.enabled` (ADR 0039).
    pub run_js_enabled: bool,
    /// `tools.run_js.scopes`: the roles and categories `run_js` is offered to.
    pub run_js_scopes: Vec<String>,
}

impl AgentFeatures {
    /// The settings as `config` has them.
    pub fn from_config(config: &Config) -> Self {
        Self {
            web_tools_enabled: config.sandbox.web_tools_enabled,
            run_js_enabled: config.tools.run_js.enabled,
            run_js_scopes: config.tools.run_js.scopes.clone(),
        }
    }
}

impl Default for AgentFeatures {
    fn default() -> Self {
        Self::from_config(&Config::default())
    }
}

/// Sets only the agent feature settings in `path`, retaining every other key
/// (including the other `[sandbox]` and `[tools.run_js]` keys) and comments.
/// `tools.run_js.scopes` is left as written when it already holds the same
/// list, and is not created while it is the default.
///
/// # Errors
/// Returns configuration validation or filesystem errors, and refuses to write
/// when the saved settings would not take effect: `[tools]` is read only from
/// user files, and a drop-in or other later layer may override the file.
pub fn save_agent_features(path: &Path, features: &AgentFeatures) -> Result<(), ConfigError> {
    save_agent_features_in_scope(path, features, crate::save_context::SaveScope::Standalone)
}

pub(crate) fn save_agent_features_in_scope(
    path: &Path,
    features: &AgentFeatures,
    scope: crate::save_context::SaveScope<'_>,
) -> Result<(), ConfigError> {
    let mut doc = crate::save::read_document(path)?;
    // Array decor may refer to spans in the original document. Keep its text
    // available while adding formatted values to the parsed array.
    let source = doc.to_string();
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
    set_scopes(&mut doc, &features.run_js_scopes, &source)?;
    let checked = crate::save::checked_text_for_save(path, &doc, scope)?;
    let effective = AgentFeatures::from_config(checked.config());
    if effective.web_tools_enabled != features.web_tools_enabled {
        return Err(not_effective(
            "sandbox.web_tools_enabled",
            "another configuration layer overrides it",
        ));
    }
    for (path, differs) in [
        (
            "tools.run_js.enabled",
            effective.run_js_enabled != features.run_js_enabled,
        ),
        (
            "tools.run_js.scopes",
            effective.run_js_scopes != features.run_js_scopes,
        ),
    ] {
        if differs {
            return Err(not_effective(
                path,
                "[tools] is read only from the user config, and a later user file must not override it",
            ));
        }
    }
    checked.write()
}

fn not_effective(path: &str, reason: &str) -> ConfigError {
    crate::save::invalid_field(path, &format!("the saved value would not apply: {reason}"))
}

fn set_scopes(doc: &mut DocumentMut, scopes: &[String], source: &str) -> Result<(), ConfigError> {
    for scope in scopes {
        crate::validate_run_js_scope(scope)
            .map_err(|message| crate::save::invalid_field("tools.run_js.scopes", &message))?;
    }
    let table = table_at(doc, &["tools", "run_js"])?;
    match table.get_mut("scopes") {
        Some(Item::Value(toml_edit::Value::Array(current))) => {
            let same = current.len() == scopes.len()
                && current
                    .iter()
                    .zip(scopes)
                    .all(|(have, want)| have.as_str() == Some(want));
            if same {
                return Ok(());
            }

            let had_trailing_comma = current.trailing_comma();
            let mut trailing = raw_text(current.trailing(), source)
                .unwrap_or_default()
                .to_owned();
            let new_value_prefix = value_prefix(current, source);
            let opening_value_prefix = opening_value_prefix(current, source);
            let mut entries = current
                .iter()
                .cloned()
                .map(|value| ScopeEntry::from_value(value, source))
                .collect::<Vec<_>>();

            // toml_edit stores a comment after a comma in the next value's
            // prefix. Associate the comment with the preceding value before
            // reordering or deleting entries.
            for index in 1..entries.len() {
                if let Some((comment, line_ending, prefix)) =
                    split_comma_comment(&entries[index].prefix)
                {
                    entries[index - 1].trailing_comment = Some((comment, line_ending));
                    entries[index].prefix = prefix;
                }
            }

            // A comment before the closing bracket can instead be part of the
            // final value's suffix (without a trailing comma).
            for entry in &mut entries {
                let suffix = entry
                    .value
                    .decor()
                    .suffix()
                    .and_then(|suffix| raw_text(suffix, source))
                    .unwrap_or_default();
                if let Some((comment, line_ending, after_comment)) = split_comma_comment(suffix) {
                    entry.trailing_comment = Some((comment, line_ending));
                    entry.value.decor_mut().set_suffix("");
                    trailing = format!("{after_comment}{trailing}");
                }
            }

            // Keep a comment immediately following the array's trailing
            // comma attached to the last original value. Comments on later
            // lines remain array-level trailing decor.
            if let (Some(last), Some((comment, line_ending, after_comment))) =
                (entries.last_mut(), split_comma_comment(&trailing))
            {
                trailing = format!("{line_ending}{after_comment}");
                last.trailing_comment = Some((comment, line_ending));
            }

            let mut remaining = entries.into_iter().map(Some).collect::<Vec<_>>();
            let mut ordered = Vec::with_capacity(scopes.len());
            for scope in scopes {
                if let Some(index) = remaining.iter().position(|entry| {
                    entry.as_ref().and_then(|entry| entry.value.as_str()) == Some(scope)
                }) {
                    ordered.push(remaining[index].take().expect("entry was found"));
                } else {
                    let prefix = if ordered.is_empty() {
                        &opening_value_prefix
                    } else {
                        &new_value_prefix
                    };
                    ordered.push(ScopeEntry::new(scope, prefix));
                }
            }

            current.clear();
            let mut previous_comment = None;
            for (index, mut entry) in ordered.into_iter().enumerate() {
                let entry_prefix = if index > 0 && entry.prefix.is_empty() {
                    &new_value_prefix
                } else {
                    &entry.prefix
                };
                let prefix = if index == 0 {
                    entry_prefix.to_owned()
                } else {
                    prefix_after_comment(previous_comment.as_ref(), entry_prefix)
                };
                entry.value.decor_mut().set_prefix(prefix);
                previous_comment = entry.trailing_comment.clone();
                current.push_formatted(entry.value);
            }

            let mut trailing_comma = had_trailing_comma;
            if let Some((comment, line_ending)) = previous_comment {
                if !line_ends_comment(&trailing) {
                    trailing = format!("{line_ending}{trailing}");
                }
                trailing = format!("{comment}{trailing}");
                trailing_comma = true;
            }
            current.set_trailing(trailing);
            current.set_trailing_comma(trailing_comma);
        }
        Some(_) => {
            return Err(crate::save::invalid_field(
                "tools.run_js.scopes",
                "must be an array of strings",
            ));
        }
        None => {
            if scopes != crate::DEFAULT_RUN_JS_SCOPES {
                let mut array = toml_edit::Array::new();
                for scope in scopes {
                    array.push(scope.as_str());
                }
                table.insert("scopes", toml_edit::value(array));
            }
        }
    }
    Ok(())
}

struct ScopeEntry {
    value: Value,
    prefix: String,
    trailing_comment: Option<(String, String)>,
}

impl ScopeEntry {
    fn from_value(value: Value, source: &str) -> Self {
        let prefix = value
            .decor()
            .prefix()
            .and_then(|prefix| raw_text(prefix, source))
            .unwrap_or_default()
            .to_owned();
        Self {
            value,
            prefix,
            trailing_comment: None,
        }
    }

    fn new(scope: &str, prefix: &str) -> Self {
        Self {
            value: Value::from(scope),
            prefix: prefix.to_owned(),
            trailing_comment: None,
        }
    }
}

/// Splits a line-end comment from whitespace decor, retaining its line ending.
fn split_comma_comment(text: &str) -> Option<(String, String, String)> {
    let newline = text.find('\n')?;
    let line_end = if newline > 0 && text.as_bytes()[newline - 1] == b'\r' {
        newline - 1
    } else {
        newline
    };
    let line = &text[..line_end];
    let comment_start = line.find('#')?;
    if !line[..comment_start]
        .chars()
        .all(|character| matches!(character, ' ' | '\t'))
    {
        return None;
    }

    Some((
        line.to_owned(),
        text[line_end..newline + 1].to_owned(),
        text[newline + 1..].to_owned(),
    ))
}

fn prefix_after_comment(trailing_comment: Option<&(String, String)>, prefix: &str) -> String {
    let Some((comment, line_ending)) = trailing_comment else {
        return prefix.to_owned();
    };

    if line_ends_comment(prefix) {
        format!("{comment}{prefix}")
    } else {
        format!("{comment}{line_ending}{prefix}")
    }
}

fn line_ends_comment(text: &str) -> bool {
    text.find('\n')
        .is_some_and(|newline| !text[..newline].contains('#'))
}

/// Gets the source text for parser-backed decor without losing owned decor.
fn raw_text<'a>(raw: &'a RawString, source: &'a str) -> Option<&'a str> {
    raw.as_str()
        .or_else(|| raw.span().and_then(|span| source.get(span)))
}

/// Whitespace to use before an appended scope, excluding any comments attached
/// to an existing value. This keeps inline and multiline arrays in their style.
fn value_prefix(array: &toml_edit::Array, source: &str) -> String {
    if let Some(last) = array.iter().last() {
        return last
            .decor()
            .prefix()
            .and_then(|prefix| raw_text(prefix, source))
            .map(prefix_whitespace)
            .filter(|prefix| !prefix.is_empty())
            .unwrap_or_else(|| " ".to_owned());
    }

    raw_text(array.trailing(), source)
        .map(empty_array_prefix)
        .unwrap_or_default()
}

fn opening_value_prefix(array: &toml_edit::Array, source: &str) -> String {
    if let Some(first) = array.iter().next() {
        return first
            .decor()
            .prefix()
            .and_then(|prefix| raw_text(prefix, source))
            .map(prefix_whitespace)
            .unwrap_or_default();
    }

    raw_text(array.trailing(), source)
        .map(empty_array_prefix)
        .unwrap_or_default()
}

fn prefix_whitespace(prefix: &str) -> String {
    if let Some(newline) = prefix.rfind('\n') {
        let indentation = prefix[newline + 1..]
            .chars()
            .take_while(|character| matches!(character, ' ' | '\t'))
            .collect::<String>();
        let line_ending = if newline > 0 && prefix.as_bytes()[newline - 1] == b'\r' {
            "\r\n"
        } else {
            "\n"
        };
        format!("{line_ending}{indentation}")
    } else if prefix
        .chars()
        .all(|character| matches!(character, ' ' | '\t'))
    {
        prefix.to_owned()
    } else {
        " ".to_owned()
    }
}

fn empty_array_prefix(trailing: &str) -> String {
    if let Some(newline) = trailing.find('\n') {
        let indentation = trailing[newline + 1..]
            .chars()
            .take_while(|character| matches!(character, ' ' | '\t'))
            .collect::<String>();
        let line_ending = if newline > 0 && trailing.as_bytes()[newline - 1] == b'\r' {
            "\r\n"
        } else {
            "\n"
        };
        format!("{line_ending}{indentation}")
    } else if trailing
        .chars()
        .all(|character| matches!(character, ' ' | '\t'))
    {
        trailing.to_owned()
    } else {
        String::new()
    }
}

/// The table at `tables`, created when missing, with implicit parents.
fn table_at<'a>(
    doc: &'a mut DocumentMut,
    tables: &[&str],
) -> Result<&'a mut dyn toml_edit::TableLike, ConfigError> {
    let mut table = doc.as_table_mut() as &mut dyn toml_edit::TableLike;
    let mut path = String::new();
    for (depth, name) in tables.iter().enumerate() {
        if depth > 0 {
            path.push('.');
        }
        path.push_str(name);
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
    Ok(table)
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
