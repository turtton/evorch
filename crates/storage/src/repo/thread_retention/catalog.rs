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
    /// Deleted by the content blob collector once no reference table names the row.
    GcUnreferenced,
    /// A content blob reference table: follows its owner row through `ON DELETE
    /// CASCADE`, and its AFTER DELETE trigger queues the blob for the collector.
    CascadeOwner,
    /// Kept on purpose; the reason is the second field of the catalog entry.
    Keep,
    /// The tombstones themselves.
    Tombstone,
}

use Treatment::{
    CascadeOwner, ConfirmDelete, Derived, GcUnreferenced, Keep, ReclaimRun, ReclaimTask,
    ReclaimThread, Tombstone,
};

/// `(table, treatment, why)`. FTS shadow tables and `sqlite_*` are excluded by the test.
pub(crate) const CATALOG: &[(&str, Treatment, &str)] = &[
    (
        "threads",
        ConfirmDelete,
        "removed when the family is confirmed; includes the archive/pin mirror columns \
         and their unnumbered-writer markers",
    ),
    (
        "thread_runs",
        ConfirmDelete,
        "removed when the family is confirmed",
    ),
    ("events", ReclaimRun, "by run_id; unattributable rows stay"),
    (
        "run_messages",
        ReclaimRun,
        "one row per message; reclaimed before the header it points at",
    ),
    (
        "run_contexts",
        ReclaimRun,
        "run header: configuration and checkpoint list",
    ),
    (
        "run_context_provenance",
        ReclaimRun,
        "sidecar of the snapshot; its message rows are reclaimed first",
    ),
    (
        "run_message_provenance",
        ReclaimRun,
        "one provenance entry per row; reclaimed before the sidecar header it points at",
    ),
    ("run_ledger", ReclaimRun, "run notes, deleted with a grant"),
    (
        "recovery_members",
        ReclaimRun,
        "runtime recovery recipes, reclaimed by run",
    ),
    (
        "recovery_claim_members",
        ReclaimRun,
        "claim incarnation receipts, reclaimed by run",
    ),
    (
        "recovery_manifests",
        Keep,
        "component metadata; member erasure trigger removes the whole component",
    ),
    (
        "recovery_claims",
        Keep,
        "claim metadata; cascades with its erased recovery component",
    ),
    ("host_input_receipts", ReclaimRun, "delivery receipts"),
    (
        "resume_intents",
        ReclaimRun,
        "incarnation, safe-point receipt and durable input queue",
    ),
    (
        "host_chat_submissions",
        ReclaimRun,
        "fingerprints of accepted chat messages, kept with their run",
    ),
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
    (
        "lanes",
        ReclaimThread,
        "the lane of a deleted thread; deleted after its reports, which cascade to its keys",
    ),
    (
        "lane_reports",
        ReclaimThread,
        "report history of a deleted lane thread",
    ),
    (
        "lane_delivery_keys",
        ReclaimThread,
        "follows its lane row through ON DELETE CASCADE",
    ),
    (
        "lane_question_routes",
        ReclaimThread,
        "requester/responder thread; also cascades with question/lane",
    ),
    (
        "lane_notifications",
        ReclaimThread,
        "source/destination thread, before receipts can disappear",
    ),
    (
        "lane_delivery_destinations",
        ReclaimThread,
        "destination thread control and lease",
    ),
    ("lane_state", Keep, "ledger/topology revision singleton"),
    (
        "lane_handoff_questions",
        ReclaimRun,
        "follows its question through ON DELETE CASCADE; ownership does not return to a later source incarnation",
    ),
    (
        "lane_handoffs",
        ReclaimThread,
        "retained while source or target survives; source tombstone and target lane delete triggers collect it only after both are gone",
    ),
    (
        "lane_management_receipts",
        ReclaimThread,
        "bounded management operation receipts of the deleted lane",
    ),
    (
        "lane_instructions",
        ReclaimThread,
        "immutable lead instruction texts of the deleted lane; a deleted lead thread does not take them",
    ),
    (
        "lane_instruction_receipts",
        ReclaimThread,
        "bounded instruction operation receipts of the deleted lane",
    ),
    (
        "pr_auto_fix_state",
        Keep,
        "PR-wide budget, policy and source guard survive thread deletion",
    ),
    (
        "pr_notices",
        Keep,
        "PR/SHA dedup survives deletion; tombstone trigger clears thread references and payload",
    ),
    (
        "pr_auto_fix_limit_events",
        Keep,
        "one exhaustion event per PR budget epoch",
    ),
    ("lane_stall_episodes", ReclaimThread, "lane episode history"),
    (
        "lane_recovery_episodes",
        ReclaimThread,
        "destination/root tombstone triggers reclaim recovery history",
    ),
    (
        "lane_verifications",
        ReclaimThread,
        "lane verification evidence and reviewer binding",
    ),
    ("lane_closes", ReclaimThread, "immutable lane close history"),
    (
        "lane_forward_receipts",
        ReclaimThread,
        "atomic question forward receipts",
    ),
    (
        "thread_pull_requests",
        ReclaimThread,
        "links and their unlink history of a deleted thread",
    ),
    (
        "thread_pull_request_candidates",
        ReclaimThread,
        "candidates suggested for a deleted thread",
    ),
    (
        "thread_pull_request_inheritance",
        ReclaimThread,
        "the inheritance row of a deleted child thread; a parent's deletion leaves its \
         children's rows alone",
    ),
    (
        "thread_pull_request_state",
        ReclaimThread,
        "lifecycle revision, notification keys and local HEAD of a deleted thread",
    ),
    (
        "pull_request_snapshots",
        Keep,
        "shared by pull request, not by thread: the facts of a key no thread references are \
         cleared when its last thread is reclaimed, but the row stays as a tiny sequence \
         counter so a fetch started before the clearing can never outrank a later one",
    ),
    (
        "lane_list_snapshots",
        Keep,
        "bounded by count (newest per lead and overall); rows of deleted lanes are skipped on read",
    ),
    (
        "lane_list_snapshot_rows",
        Keep,
        "follows its snapshot through ON DELETE CASCADE",
    ),
    ("tasks", ReclaimTask, "tasks of the family's runs"),
    ("task_links", ReclaimTask, "links of those tasks"),
    (
        "task_queue_ledger",
        ReclaimTask,
        "deleted with a grant, found by JSON payload",
    ),
    (
        "content_blobs",
        GcUnreferenced,
        "collected once no content_blob_refs_* table names the blob",
    ),
    (
        "content_blob_refs_event",
        CascadeOwner,
        "follows its event row; the trigger queues the blob",
    ),
    (
        "content_blob_refs_header",
        CascadeOwner,
        "follows its run header; the trigger queues the blob",
    ),
    (
        "content_blob_refs_message",
        CascadeOwner,
        "follows its message row; the trigger queues the blob",
    ),
    (
        "content_blob_gc_queue",
        Keep,
        "resumable collector work list; the collector empties it",
    ),
    ("usage_requests", Keep, "cost accounting is kept"),
    ("usage_daily", Keep, "cost accounting is kept"),
    ("usage_run_threads", Keep, "cost attribution is kept"),
    ("learning_claims", Keep, "learning is kept"),
    ("learning_claim_roots", Keep, "learning is kept"),
    ("improvement_candidates", Keep, "learning is kept"),
    ("improvement_intake", Keep, "learning is kept"),
    (
        "improvement_observations",
        Keep,
        "cross-owner replay receipts outlive threads and candidate retention",
    ),
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
    ("retention_unresolved", Keep, "retention bookkeeping"),
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

    #[test]
    fn content_blob_reference_tables_cascade_with_their_owner() {
        // Given: the latest schema and the catalog.
        let database = Database::open_in_memory().unwrap();
        let tables = crate::repo::content_blobs::ref_tables(&database.conn).unwrap();

        // Then: every reference table in the schema is catalogued as following its
        // owner, and no other entry claims that treatment, so the collector's table
        // discovery and the catalog cannot drift apart.
        let cascading: Vec<&str> = CATALOG
            .iter()
            .filter(|(_, treatment, _)| *treatment == super::Treatment::CascadeOwner)
            .map(|(name, ..)| *name)
            .collect();
        assert_eq!(cascading, tables);
        assert!(
            CATALOG
                .iter()
                .filter(|(name, ..)| name.starts_with(crate::body_store::REF_TABLE_PREFIX))
                .all(|(_, treatment, _)| *treatment == super::Treatment::CascadeOwner)
        );
    }
}
