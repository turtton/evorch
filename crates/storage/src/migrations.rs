//! SQLite スキーマ移行を管理します。

use rusqlite::Connection;

use crate::StorageError;

#[path = "migrations/sql.rs"]
mod sql;

#[path = "migrations/backfill.rs"]
pub(crate) mod backfill;

pub(crate) const MIGRATIONS: &[&str] = &[
    sql::V1,
    sql::V2,
    include_str!("migrations/v3.sql"),
    include_str!("migrations/v4.sql"),
    include_str!("migrations/v5.sql"),
    include_str!("migrations/v6.sql"),
    include_str!("migrations/v7.sql"),
    include_str!("migrations/v8.sql"),
    include_str!("migrations/v9.sql"),
    include_str!("migrations/v10.sql"),
    include_str!("migrations/v11.sql"),
    include_str!("migrations/v12.sql"),
    include_str!("migrations/v13.sql"),
    include_str!("migrations/v14.sql"),
    include_str!("migrations/v15.sql"),
    include_str!("migrations/v16.sql"),
    include_str!("migrations/v17.sql"),
    include_str!("migrations/v18.sql"),
    include_str!("migrations/v19.sql"),
    include_str!("migrations/v20.sql"),
    include_str!("migrations/v21.sql"),
    include_str!("migrations/v22.sql"),
    include_str!("migrations/v23.sql"),
    include_str!("migrations/v24.sql"),
    include_str!("migrations/v25.sql"),
    include_str!("migrations/v26.sql"),
    include_str!("migrations/v27.sql"),
    include_str!("migrations/v28.sql"),
    include_str!("migrations/v29.sql"),
    include_str!("migrations/v30.sql"),
    include_str!("migrations/v31.sql"),
    include_str!("migrations/v32.sql"),
    include_str!("migrations/v33.sql"),
    include_str!("migrations/v34.sql"),
    include_str!("migrations/v35.sql"),
    include_str!("migrations/v36.sql"),
    include_str!("migrations/v37.sql"),
    include_str!("migrations/v38.sql"),
    include_str!("migrations/v39.sql"),
    concat!(
        include_str!("migrations/v40_drop.sql"),
        include_str!("migrations/v40_prepare.sql")
    ),
    include_str!("migrations/v41.sql"),
    include_str!("migrations/v42.sql"),
    concat!(
        include_str!("migrations/v43_drop.sql"),
        include_str!("migrations/v43_prepare.sql")
    ),
    include_str!("migrations/event_bodies.sql"),
];

/// Migration v40 without its last step: the new tables, the header columns and the
/// triggers. The conversion command runs it on a v39 database that still has
/// `messages_json`, while it moves the messages.
pub(crate) const V40_PREPARE: &str = include_str!("migrations/v40_prepare.sql");

/// The step of migration v40 that removes `run_contexts.messages_json`; the conversion
/// command runs it once every run is converted.
pub(crate) const V40_DROP: &str = include_str!("migrations/v40_drop.sql");

/// Migration v43 without its last step: the header columns, the message table and the
/// triggers. The conversion command runs it while it moves the maps.
pub(crate) const V43_PREPARE: &str = include_str!("migrations/v43_prepare.sql");

/// The step of migration v43 that removes `run_context_provenance.map_json`; the
/// conversion command runs it once every map is converted.
pub(crate) const V43_DROP: &str = include_str!("migrations/v43_drop.sql");

/// Migration 44 (event bodies as tokens): the new `events` columns, the event reference
/// table and the accounting triggers. The conversion command runs it as its "events
/// prepare" step, after the runs and the maps are converted and before the events are.
pub(crate) const EVENT_BODIES: &str = include_str!("migrations/event_bodies.sql");

/// The first schema version whose `events` rows may hold their payload in token form
/// (`body_format` 1). A database below it (and not empty) is converted by the
/// conversion command, never by a normal start.
pub(crate) const EVENT_BODIES_VERSION: u32 = 44;

/// The first schema version that stores a run's messages in `run_messages`. A database
/// below it (and not empty) holds them in `run_contexts.messages_json`: this build
/// never opens it. Only the conversion command may turn one into the current format.
pub(crate) const MESSAGE_ROWS_VERSION: u32 = 40;

/// The first schema version that stores a provenance map in `run_message_provenance`
/// rows. A database below it (and not empty) holds the whole map in
/// `run_context_provenance.map_json`: only the conversion command opens one.
pub(crate) const PROVENANCE_ROWS_VERSION: u32 = 43;

/// Refuses a database that is not in the current conversation format, before anything
/// writes to it (no pragma, no migration, no row). A new (empty) database passes: it is
/// created in the current format by [`apply_migrations`].
///
/// A database whose `user_version` is current but whose `run_contexts` still has the old
/// column, or lacks the header columns, is a conversion that stopped half way, and is
/// refused the same way.
pub(crate) fn require_current_format(conn: &Connection) -> Result<(), StorageError> {
    let found = conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))?;
    let supported =
        u32::try_from(MIGRATIONS.len()).map_err(|_| StorageError::OutOfRange("migration count"))?;
    if found == 0 {
        return Ok(());
    }
    if found > supported {
        return Err(StorageError::SchemaTooNew { found, supported });
    }
    if found < MESSAGE_ROWS_VERSION {
        return Err(StorageError::ConversionRequired {
            found,
            detail: "a run's messages are one column of run_contexts",
        });
    }
    if found < PROVENANCE_ROWS_VERSION {
        return Err(StorageError::ConversionRequired {
            found,
            detail: "a run's provenance map is one column of run_context_provenance",
        });
    }
    if found < EVENT_BODIES_VERSION {
        return Err(StorageError::ConversionRequired {
            found,
            detail: "the payloads of events are not in the token format",
        });
    }
    let columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('run_contexts')")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let has = |name: &str| columns.iter().any(|column| column == name);
    if has("messages_json") || !has("body_format") || !has("header_rev") {
        return Err(StorageError::ConversionRequired {
            found,
            detail: "run_contexts is not in the header format (an interrupted conversion)",
        });
    }
    let provenance_columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('run_context_provenance')")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let has_provenance = |name: &str| provenance_columns.iter().any(|column| column == name);
    if has_provenance("map_json") || !has_provenance("map_message_count") {
        return Err(StorageError::ConversionRequired {
            found,
            detail: "run_context_provenance is not in the header format (an interrupted conversion)",
        });
    }
    let event_columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('events')")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let has_event = |name: &str| event_columns.iter().any(|column| column == name);
    if !has_event("body_format") || !has_event("blob_bytes") {
        return Err(StorageError::ConversionRequired {
            found,
            detail: "events is not in the token format (an interrupted conversion)",
        });
    }
    Ok(())
}

pub(crate) fn apply_migrations(conn: &Connection) -> Result<(), StorageError> {
    require_current_format(conn)?;
    apply_pending(conn)
}

/// Applies every migration above the database's `user_version`, with no format check.
/// Tests use it to build the old shapes by hand and then migrate them.
#[cfg(any(test, feature = "unchecked-open"))]
pub(crate) fn apply_migrations_unchecked(conn: &Connection) -> Result<(), StorageError> {
    apply_pending(conn)
}

fn apply_pending(conn: &Connection) -> Result<(), StorageError> {
    let supported =
        u32::try_from(MIGRATIONS.len()).map_err(|_| StorageError::OutOfRange("migration count"))?;
    apply_up_to(conn, supported)
}

/// Applies the migrations above the database's `user_version` up to and including
/// `target`, each in its own transaction, with no format check. The conversion command
/// uses it to bring an older database to version 39 before it converts the messages.
pub(crate) fn apply_up_to(conn: &Connection, target: u32) -> Result<(), StorageError> {
    let current = conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))?;
    let supported =
        u32::try_from(MIGRATIONS.len()).map_err(|_| StorageError::OutOfRange("migration count"))?;
    if current > supported {
        return Err(StorageError::SchemaTooNew {
            found: current,
            supported,
        });
    }
    let target_index =
        usize::try_from(target).map_err(|_| StorageError::OutOfRange("target migration"))?;
    let current_index = usize::try_from(current)
        .map_err(|_| StorageError::OutOfRange("current migration version"))?;
    for (index, migration) in MIGRATIONS
        .iter()
        .enumerate()
        .take(target_index)
        .skip(current_index)
    {
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

/// Reverts migration v44 (see `tests/support/undo_v44.sql`); apply it before [`UNDO_V43`].
#[cfg(test)]
pub(crate) const UNDO_V44: &str = include_str!("../tests/support/undo_v44.sql");

/// Reverts migration v43 (see `tests/support/undo_v43.sql`); apply it before [`UNDO_V40`].
#[cfg(test)]
pub(crate) const UNDO_V43: &str = include_str!("../tests/support/undo_v43.sql");

/// Reverts migration v40 (see `tests/support/undo_v40.sql`); apply it before [`UNDO_V35`].
#[cfg(test)]
pub(crate) const UNDO_V40: &str = include_str!("../tests/support/undo_v40.sql");

/// Reverts migration v35 on a database whose `threads` table is kept, for tests
/// that rebuild an older schema by hand. `thread_index_runs` is not restored:
/// the caller drops it, as v35 rebuilds it from scratch.
#[cfg(test)]
pub(crate) const UNDO_V35: &str = r#"
DROP TABLE content_blob_gc_queue; DROP TABLE content_blobs;
DROP INDEX idx_events_orchestrator_unattributed;
DROP TRIGGER fence_run_contexts_insert;
DROP TRIGGER fence_run_contexts_update;
DROP TRIGGER fence_run_context_provenance_insert;
DROP TRIGGER fence_run_context_provenance_update;
DROP TRIGGER fence_run_ledger_insert;
DROP TRIGGER fence_host_input_receipts_insert;
DROP TRIGGER fence_host_input_receipts_update;
DROP TRIGGER fence_review_evidence_insert;
DROP TRIGGER fence_review_evidence_update;
DROP TRIGGER fence_review_lineage_edges_insert;
DROP TRIGGER fence_review_lineage_edges_update;
DROP TRIGGER fence_assistant_references_insert;
DROP TRIGGER fence_assistant_references_update;
DROP TRIGGER fence_user_questions_insert;
DROP TRIGGER fence_user_questions_update;
DROP TRIGGER fence_user_question_links_insert;
DROP TRIGGER fence_user_question_links_update;
DROP TRIGGER fence_conversation_blocks_insert;
DROP TRIGGER fence_conversation_blocks_update;
DROP TRIGGER fence_thread_runs_insert;
DROP TRIGGER fence_thread_runs_update;
DROP TRIGGER fence_thread_start_states_insert;
DROP TRIGGER fence_thread_start_states_update;
DROP TRIGGER fence_thread_start_untracked_insert;
DROP TRIGGER fence_thread_start_untracked_update;
DROP TRIGGER fence_eval_samples_insert;
DROP TRIGGER fence_eval_samples_update;
DROP TRIGGER fence_usage_run_threads_insert;
DROP TRIGGER fence_usage_run_threads_update;
DROP TRIGGER fence_threads_insert;
DROP TRIGGER fence_threads_update;
DROP TRIGGER fence_tasks_insert;
DROP TRIGGER fence_tasks_update;
DROP TRIGGER fence_task_links_insert;
DROP TRIGGER fence_thread_index_runs_insert;
DROP TRIGGER fence_thread_index_runs_update;
DROP TRIGGER fence_task_queue_ledger_insert;
DROP TRIGGER fence_events_insert;
DROP INDEX idx_tasks_parent_run; DROP INDEX idx_assistant_references_source;
DROP INDEX idx_run_contexts_chat_thread; DROP INDEX idx_threads_archived;
DROP INDEX idx_threads_root_run; DROP INDEX idx_threads_escalation_source;
ALTER TABLE threads DROP COLUMN archived_at_ns; ALTER TABLE threads DROP COLUMN pinned;
ALTER TABLE threads DROP COLUMN archive_revision; ALTER TABLE threads DROP COLUMN pin_revision;
DROP TRIGGER run_ledger_no_delete;
CREATE TRIGGER run_ledger_no_delete BEFORE DELETE ON run_ledger
BEGIN SELECT RAISE(ABORT, 'run ledger is append-only'); END;
DROP TRIGGER task_queue_no_delete;
CREATE TRIGGER task_queue_no_delete BEFORE DELETE ON task_queue_ledger
BEGIN SELECT RAISE(ABORT, 'task queue ledger is append-only'); END;
DROP TABLE artifact_refs; DROP TABLE gc_state; DROP TABLE gc_unknown_meta;
DROP TABLE reclaim_jobs; DROP TABLE reclaim_oversized;
DROP TABLE retention_binding; DROP TABLE retention_identity;
DROP TABLE retention_unresolved; DROP TABLE retention_staging; DROP TABLE retention_staging_state;
DROP TABLE run_ledger_delete_grants; DROP TABLE task_queue_delete_grants;
DROP TABLE run_tombstones; DROP TABLE task_tombstones; DROP TABLE thread_tombstones;
"#;

#[cfg(test)]
mod v41_tests;
