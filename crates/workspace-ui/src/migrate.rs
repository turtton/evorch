//! Workspace JSON schema migrations.

use serde_json::Value;

use crate::{LayoutError, WORKSPACE_SCHEMA_VERSION};

type MigrationResult = Result<Value, LayoutError>;

const MIGRATIONS: &[fn(Value) -> MigrationResult] = &[migrate_v1_to_v2];

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
