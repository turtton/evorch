//! SQLite スキーマ移行を管理します。

use rusqlite::Connection;

use crate::StorageError;

#[path = "migrations/sql.rs"]
mod sql;

pub(crate) const MIGRATIONS: &[&str] = &[sql::V1];

pub(crate) fn apply_migrations(conn: &Connection) -> Result<(), StorageError> {
    let current = conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))?;
    let supported =
        u32::try_from(MIGRATIONS.len()).map_err(|_| StorageError::OutOfRange("migration count"))?;
    if current > supported {
        return Err(StorageError::SchemaTooNew {
            found: current,
            supported,
        });
    }

    let current_index = usize::try_from(current)
        .map_err(|_| StorageError::OutOfRange("current migration version"))?;
    for (index, migration) in MIGRATIONS.iter().enumerate().skip(current_index) {
        let version =
            u32::try_from(index + 1).map_err(|_| StorageError::OutOfRange("migration version"))?;
        let transaction = conn
            .unchecked_transaction()
            .map_err(|error| migration_error(version, error))?;
        transaction
            .execute_batch(migration)
            .map_err(|error| migration_error(version, error))?;
        transaction
            .pragma_update(None, "user_version", version)
            .map_err(|error| migration_error(version, error))?;
        transaction
            .commit()
            .map_err(|error| migration_error(version, error))?;
    }

    Ok(())
}

fn migration_error(version: u32, error: rusqlite::Error) -> StorageError {
    StorageError::Migration {
        version,
        message: error.to_string(),
    }
}
