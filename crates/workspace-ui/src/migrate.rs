//! Workspace JSON schema migrations.

use serde_json::Value;

use crate::{LayoutError, WORKSPACE_SCHEMA_VERSION};

type MigrationResult = Result<Value, LayoutError>;

const MIGRATIONS: &[fn(Value) -> MigrationResult] = &[migrate_v1_to_v2, migrate_v2_to_v3];

/// Migrates a versioned workspace JSON value to the current schema.
///
/// # Errors
/// Returns a typed layout error for missing, invalid, zero, or future versions.
pub fn run(value: Value) -> MigrationResult {
    let raw_version = value.get("version").ok_or_else(|| LayoutError::Migration {
        detail: "missing version key".to_owned(),
    })?;
    let version = raw_version
        .as_u64()
        .and_then(|raw| u32::try_from(raw).ok())
        .ok_or_else(|| LayoutError::Migration {
            detail: format!("version key must be a non-negative integer, got: {raw_version}"),
        })?;

    if version > WORKSPACE_SCHEMA_VERSION {
        return Err(LayoutError::UnsupportedVersion {
            found: version,
            supported: WORKSPACE_SCHEMA_VERSION,
        });
    }
    if version == 0 {
        return Err(LayoutError::Migration {
            detail: "version key must be at least 1".to_owned(),
        });
    }

    let mut migrated = value;
    for migration in &MIGRATIONS[version as usize - 1..] {
        migrated = migration(migrated)?;
    }
    Ok(migrated)
}

fn migrate_v1_to_v2(mut value: Value) -> MigrationResult {
    let object = value
        .as_object_mut()
        .ok_or_else(|| LayoutError::Migration {
            detail: "versioned workspace root must be an object".to_owned(),
        })?;
    object.insert("version".to_owned(), Value::from(2));
    Ok(value)
}

fn migrate_v2_to_v3(mut value: Value) -> MigrationResult {
    let object = value
        .as_object_mut()
        .ok_or_else(|| LayoutError::Migration {
            detail: "versioned workspace root must be an object".to_owned(),
        })?;
    let mut removed = std::collections::BTreeSet::new();
    if let Some(panels) = object.get_mut("panels").and_then(Value::as_object_mut) {
        panels.retain(|id, panel| {
            let keep = !matches!(
                panel.get("kind").and_then(Value::as_str),
                Some("goal" | "merge_approval")
            );
            if !keep {
                removed.insert(id.clone());
            }
            keep
        });
    }
    if let Some(main) = object.get_mut("main")
        && !prune_window(main, &removed)
    {
        return Err(LayoutError::Migration {
            detail: "removing obsolete panels leaves the main window empty".to_owned(),
        });
    }
    if let Some(windows) = object
        .get_mut("extra_windows")
        .and_then(Value::as_array_mut)
    {
        windows.retain_mut(|window| prune_window(window, &removed));
    }
    object.insert("version".to_owned(), Value::from(3));
    Ok(value)
}

fn prune_window(window: &mut Value, removed: &std::collections::BTreeSet<String>) -> bool {
    if let Some(floating) = window.get_mut("floating").and_then(Value::as_array_mut) {
        floating.retain_mut(|pane| {
            pane.get_mut("node")
                .is_none_or(|node| prune_node(node, removed))
        });
    }
    let root_survives = window
        .get_mut("root")
        .is_none_or(|root| prune_node(root, removed));
    if root_survives {
        return true;
    }
    if let Some(floating) = window.get_mut("floating").and_then(Value::as_array_mut)
        && !floating.is_empty()
    {
        let mut pane = floating.remove(0);
        if let Some(node) = pane.get_mut("node") {
            window["root"] = node.take();
            return true;
        }
    }
    false
}

fn prune_node(node: &mut Value, removed: &std::collections::BTreeSet<String>) -> bool {
    match node.get("type").and_then(Value::as_str) {
        Some("tabs") => {
            let Some(panels) = node.get_mut("panels").and_then(Value::as_array_mut) else {
                return true;
            };
            panels.retain(|id| id.as_str().is_none_or(|id| !removed.contains(id)));
            let len = panels.len();
            if len == 0 {
                return false;
            }
            if let Some(active) = node.get("active").and_then(Value::as_u64) {
                node["active"] =
                    Value::from(active.min(u64::try_from(len - 1).unwrap_or(u64::MAX)));
            }
            true
        }
        Some("split") => {
            let first = node
                .get_mut("first")
                .is_none_or(|side| prune_node(side, removed));
            let second = node
                .get_mut("second")
                .is_none_or(|side| prune_node(side, removed));
            match (first, second) {
                (true, true) => true,
                (false, false) => false,
                (true, false) => {
                    *node = node["first"].take();
                    true
                }
                (false, true) => {
                    *node = node["second"].take();
                    true
                }
            }
        }
        _ => true,
    }
}
