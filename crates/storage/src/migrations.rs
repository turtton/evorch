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
];

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

/// Reverts migration v35 on a database whose `threads` table is kept, for tests
/// that rebuild an older schema by hand. `thread_index_runs` is not restored:
/// the caller drops it, as v35 rebuilds it from scratch.
#[cfg(test)]
pub(crate) const UNDO_V35: &str = r#"
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
DROP TABLE retention_staging; DROP TABLE retention_staging_state;
DROP TABLE run_ledger_delete_grants; DROP TABLE task_queue_delete_grants;
DROP TABLE run_tombstones; DROP TABLE task_tombstones; DROP TABLE thread_tombstones;
"#;
