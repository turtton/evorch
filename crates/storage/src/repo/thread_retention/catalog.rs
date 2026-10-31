//! Every table has a retention treatment. A test compares this list with the
//! schema, so a new table cannot be added without deciding what deleting an
//! archived thread does to it.

/// What a confirmed deletion does to a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Treatment {
    /// Deleted by the confirming transaction itself.
    ConfirmDelete,
    /// Deleted by bounded reclaim steps, found by run.
    ReclaimRun,
    /// Deleted by bounded reclaim steps, found by thread.
    ReclaimThread,
    /// Deleted by bounded reclaim steps, found by task.
    ReclaimTask,
    /// Rebuilt from another table's rows (full-text index); follows its source.
    Derived,
    /// Kept on purpose; the reason is the second field of the catalog entry.
    Keep,
    /// The tombstones themselves.
    Tombstone,
}

use Treatment::{ConfirmDelete, Derived, Keep, ReclaimRun, ReclaimTask, ReclaimThread, Tombstone};

/// `(table, treatment, why)`. FTS shadow tables and `sqlite_*` are excluded by the test.
pub(crate) const CATALOG: &[(&str, Treatment, &str)] = &[
    (
        "threads",
        ConfirmDelete,
        "removed when the family is confirmed",
    ),
    (
        "thread_runs",
        ConfirmDelete,
        "removed when the family is confirmed",
    ),
    ("events", ReclaimRun, "by run_id; unattributable rows stay"),
    ("run_contexts", ReclaimRun, "conversation snapshots"),
    (
        "run_context_provenance",
        ReclaimRun,
        "sidecar of the snapshot",
    ),
    ("run_ledger", ReclaimRun, "run notes, deleted with a grant"),
    ("host_input_receipts", ReclaimRun, "delivery receipts"),
    ("review_evidence", ReclaimRun, "review evidence"),
    ("review_lineage_edges", ReclaimRun, "review lineage"),
    ("assistant_references", ReclaimRun, "approved references"),
    ("user_questions", ReclaimRun, "questions asked by the run"),
    ("user_question_links", ReclaimRun, "question recipients"),
    ("conversation_blocks", ReclaimRun, "search index text"),
    (
        "conversation_fts",
        Derived,
        "follows conversation_blocks through triggers",
    ),
    ("thread_index_runs", ReclaimRun, "ends as a deleted row"),
    (
        "thread_start_untracked",
        ReclaimThread,
        "captured untracked files",
    ),
    ("thread_start_states", ReclaimThread, "captured start state"),
    ("tasks", ReclaimTask, "tasks of the family's runs"),
    ("task_links", ReclaimTask, "links of those tasks"),
    (
        "task_queue_ledger",
        ReclaimTask,
        "deleted with a grant, found by JSON payload",
    ),
    ("usage_requests", Keep, "cost accounting is kept"),
    ("usage_daily", Keep, "cost accounting is kept"),
    ("usage_run_threads", Keep, "cost attribution is kept"),
    ("learning_claims", Keep, "learning is kept"),
    ("learning_claim_roots", Keep, "learning is kept"),
    ("improvement_candidates", Keep, "learning is kept"),
    ("improvement_intake", Keep, "learning is kept"),
    ("memory_ledger", Keep, "learning is kept"),
    ("memory_entries", Keep, "learning is kept"),
    ("memory_fts", Keep, "learning is kept"),
    ("thread_index_requests", Keep, "request history"),
    (
        "thread_index_request_targets",
        Keep,
        "request history; pending targets close as gaps",
    ),
    ("thread_index_state", Keep, "index job singleton"),
    ("thread_index_lease", Keep, "index job singleton"),
    (
        "eval_samples",
        Keep,
        "an evaluation sample is never confirmed for deletion",
    ),
    ("sessions", Keep, "session totals follow event deletion"),
    (
        "event_session_bytes",
        Keep,
        "accounting follows event deletion",
    ),
    ("event_day_bytes", Keep, "accounting follows event deletion"),
    ("downsampled_metrics", Keep, "metrics"),
    ("catalog_updates", Keep, "catalog history"),
    ("team_ledger", Keep, "team history"),
    ("messages", Keep, "legacy table"),
    ("agent_runs", Keep, "legacy table"),
    ("retention_identity", Keep, "retention bookkeeping"),
    ("retention_binding", Keep, "retention bookkeeping"),
    ("retention_staging_state", Keep, "retention bookkeeping"),
    ("retention_staging", Keep, "retention bookkeeping"),
    ("reclaim_jobs", Keep, "retention bookkeeping"),
    ("reclaim_oversized", Keep, "retention bookkeeping"),
    ("run_ledger_delete_grants", Keep, "retention bookkeeping"),
    ("task_queue_delete_grants", Keep, "retention bookkeeping"),
    ("artifact_refs", Keep, "artifact collector input"),
    ("gc_state", Keep, "artifact collector state"),
    ("gc_unknown_meta", Keep, "artifact collector state"),
    ("thread_tombstones", Tombstone, "the fence"),
    ("run_tombstones", Tombstone, "the fence"),
    ("task_tombstones", Tombstone, "the fence"),
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::CATALOG;
    use crate::Database;

    #[test]
    fn every_table_has_a_retention_treatment() {
        // Given: the latest schema.
        let database = Database::open_in_memory().unwrap();
        let mut statement = database
            .conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
                 AND name NOT GLOB 'conversation_fts_*' AND name NOT GLOB 'memory_fts_*'",
            )
            .unwrap();
        let tables: BTreeSet<String> = statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        // Then: the catalog lists exactly those tables, once each.
        let catalogued: BTreeSet<String> = CATALOG
            .iter()
            .map(|(name, ..)| (*name).to_owned())
            .collect();
        assert_eq!(catalogued.len(), CATALOG.len(), "a table is listed twice");
        assert_eq!(catalogued, tables);
    }
}
