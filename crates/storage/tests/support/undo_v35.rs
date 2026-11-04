//! Shared by the migration tests that rebuild an older schema by hand.

use rusqlite::Connection;

fn triggers(connection: &Connection) -> Vec<String> {
    let mut statement = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'trigger' AND name LIKE 'fence\\_%' ESCAPE '\\'")
        .unwrap();
    statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// Restores the whole-conversation `run_contexts` shape migration v40 replaced (after
/// restoring the whole-map `run_context_provenance` shape migration v42 replaced).
pub fn undo_v40(connection: &Connection) {
    connection
        .execute_batch(include_str!("undo_v42.sql"))
        .unwrap();
    connection
        .execute_batch(include_str!("undo_v40.sql"))
        .unwrap();
}

/// Removes the content blob tables migration v39 added, after undoing v40.
pub fn undo_v39(connection: &Connection) {
    undo_v40(connection);
    connection
        .execute_batch(
            "DROP TABLE IF EXISTS content_blob_gc_queue; DROP TABLE IF EXISTS content_blobs;",
        )
        .unwrap();
}

/// Restores what migration v38 dropped (the unnumbered-writer markers) and removes what it
/// added (the table of unresolved families), after undoing v39.
pub fn undo_v38(connection: &Connection) {
    undo_v39(connection);
    connection
        .execute_batch(
            "DROP TABLE IF EXISTS retention_unresolved;
             ALTER TABLE threads ADD COLUMN archive_unnumbered INTEGER NOT NULL DEFAULT 0
                 CHECK (archive_unnumbered IN (0, 1));
             ALTER TABLE threads ADD COLUMN pin_unnumbered INTEGER NOT NULL DEFAULT 0
                 CHECK (pin_unnumbered IN (0, 1));",
        )
        .unwrap();
}

/// Removes the columns migration v37 added to a table the older schema keeps.
pub fn undo_v37(connection: &Connection) {
    undo_v38(connection);
    connection
        .execute_batch(
            "ALTER TABLE gc_state DROP COLUMN ref_cursor;
             ALTER TABLE gc_state DROP COLUMN meta_count;
             ALTER TABLE gc_state DROP COLUMN meta_hash;",
        )
        .unwrap();
}

/// Removes the columns migrations v36 to v38 added to tables the older schema keeps.
/// (Its indexes are created with `IF NOT EXISTS`, so replaying leaves them be.)
pub fn undo_v36(connection: &Connection) {
    undo_v37(connection);
    connection
        .execute_batch(
            "DROP INDEX IF EXISTS idx_thread_index_runs_archived_pending;
             ALTER TABLE thread_index_runs DROP COLUMN purge_deferred;
             ALTER TABLE thread_index_state DROP COLUMN archive_backfill_done;
             ALTER TABLE thread_index_state DROP COLUMN archive_backfill_thread;
             ALTER TABLE thread_index_state DROP COLUMN archive_backfill_run;",
        )
        .unwrap();
}

/// Removes everything migrations v35 to v38 changed to the tables a hand-built
/// older schema keeps, so that the test can replay the migrations from its version.
pub fn undo_v35(connection: &Connection) {
    undo_v36(connection);
    for name in triggers(connection) {
        connection
            .execute_batch(&format!("DROP TRIGGER {name};"))
            .unwrap();
    }
    connection
        .execute_batch(
            "DROP INDEX IF EXISTS idx_events_orchestrator_unattributed;
             DROP INDEX IF EXISTS idx_tasks_parent_run;
             DROP INDEX IF EXISTS idx_assistant_references_source;
             DROP INDEX IF EXISTS idx_run_contexts_chat_thread;
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
             DROP TABLE run_tombstones; DROP TABLE task_tombstones; DROP TABLE thread_tombstones;",
        )
        .unwrap();
    let has_mirror_columns: bool = connection
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('threads') WHERE name = 'archived_at_ns'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    if has_mirror_columns {
        connection
            .execute_batch(
                "DROP INDEX idx_threads_archived; DROP INDEX idx_threads_root_run;
                 DROP INDEX idx_threads_escalation_source;
                 ALTER TABLE threads DROP COLUMN archived_at_ns;
                 ALTER TABLE threads DROP COLUMN pinned;
                 ALTER TABLE threads DROP COLUMN archive_revision;
                 ALTER TABLE threads DROP COLUMN pin_revision;
                 ALTER TABLE threads DROP COLUMN archive_unnumbered;
                 ALTER TABLE threads DROP COLUMN pin_unnumbered;",
            )
            .unwrap();
    }
}
