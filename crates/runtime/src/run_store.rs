//! run の永続化 writer と読み取り接続。

use std::sync::Mutex;

use storage::{
    Database, RunContextRecord, RunLedgerEntry, StorageConfig, StorageError, StorageHandle,
};

use crate::RunId;

pub struct RunStore {
    pub(crate) db_path: std::path::PathBuf,
    pub(crate) handle: StorageHandle,
    database: Mutex<Database>,
    /// Largest run ID already reserved in storage, so new IDs never reuse it.
    pub(crate) latest_run_id: Option<RunId>,
    pub(crate) restore_gate: Mutex<()>,
}

impl RunStore {
    /// One read on the shared connection. Thread search composes its scoped
    /// queries here; callers authorize before reading anything.
    pub(crate) fn with_database<T>(
        &self,
        read: impl FnOnce(&Database) -> Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        let database = self
            .database
            .lock()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        read(&database)
    }

    /// Thread definition by exact ID; no visibility check is performed here.
    pub(crate) fn thread(
        &self,
        id: &str,
    ) -> Result<Option<storage::threads::ThreadRow>, StorageError> {
        self.database
            .lock()
            .map_err(|e| StorageError::Io(e.to_string()))?
            .thread(id)
    }

    /// Resolve a saved run's conversation, including legacy parent/name fallback.
    pub(crate) fn thread_of_run(
        &self,
        id: &str,
    ) -> Result<Option<storage::threads::ThreadRunRow>, StorageError> {
        self.database
            .lock()
            .map_err(|e| StorageError::Io(e.to_string()))?
            .thread_of_run(id)
    }

    /// Page through exact canonical-project matches; unknown roots never match.
    pub(crate) fn threads_in_project(
        &self,
        root: &str,
        cursor: Option<&storage::threads::ThreadCursor>,
        limit: usize,
    ) -> Result<storage::threads::ThreadPage, StorageError> {
        self.database
            .lock()
            .map_err(|e| StorageError::Io(e.to_string()))?
            .threads_in_project(root, cursor, limit)
    }

    pub(crate) fn thread_cursor_token(
        &self,
        cursor: &storage::threads::ThreadCursor,
    ) -> Result<String, StorageError> {
        self.database
            .lock()
            .map_err(|e| StorageError::Io(e.to_string()))?
            .thread_cursor_token(cursor)
    }

    pub(crate) fn thread_cursor_from_token(
        &self,
        token: &str,
    ) -> Result<Option<storage::threads::ThreadCursor>, StorageError> {
        self.database
            .lock()
            .map_err(|e| StorageError::Io(e.to_string()))?
            .thread_cursor_from_token(token)
    }

    /// Children recorded by fork/rewind/escalation metadata.
    pub(crate) fn thread_children(
        &self,
        id: &str,
    ) -> Result<Vec<storage::threads::ThreadRow>, StorageError> {
        self.database
            .lock()
            .map_err(|e| StorageError::Io(e.to_string()))?
            .thread_children(id)
    }

    /// Saved runs of a conversation, including delegated children.
    pub(crate) fn runs_of_thread(
        &self,
        id: &str,
    ) -> Result<Vec<storage::threads::ThreadRunRow>, StorageError> {
        self.database
            .lock()
            .map_err(|e| StorageError::Io(e.to_string()))?
            .runs_of_thread(id)
    }

    /// Load histories only after the caller has authorized their conversation.
    pub(crate) fn run_contexts_for(
        &self,
        ids: &[String],
    ) -> Result<Vec<RunContextRecord>, StorageError> {
        self.database
            .lock()
            .map_err(|e| StorageError::Io(e.to_string()))?
            .run_contexts_for(ids)
    }

    /// Latest working note revisions; never evidence for visibility.
    pub(crate) fn latest_ledgers(
        &self,
        ids: &[String],
    ) -> Result<Vec<RunLedgerEntry>, StorageError> {
        self.database
            .lock()
            .map_err(|e| StorageError::Io(e.to_string()))?
            .latest_ledgers(ids)
    }

    /// Terminal summaries without decoding full histories.
    pub(crate) fn terminal_summaries(
        &self,
        ids: &[String],
    ) -> Result<Vec<storage::threads::RunTerminalSummary>, StorageError> {
        self.database
            .lock()
            .map_err(|e| StorageError::Io(e.to_string()))?
            .terminal_summaries(ids)
    }

    /// Saved state without parsing configuration or conversation JSON.
    pub(crate) fn run_context_headers(
        &self,
        ids: &[String],
    ) -> Result<Vec<storage::RunContextHeader>, StorageError> {
        self.database
            .lock()
            .map_err(|e| StorageError::Io(e.to_string()))?
            .run_context_headers(ids)
    }

    /// Questions delivered to these runs, deduplicated by durable question ID.
    pub(crate) fn user_questions_for_runs(
        &self,
        ids: &[String],
    ) -> Result<Vec<event_bus::UserQuestion>, StorageError> {
        let db = self
            .database
            .lock()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        for id in ids {
            for question in db.user_questions_for_run(id)? {
                if seen.insert(question.id.clone()) {
                    result.push(question);
                }
            }
        }
        Ok(result)
    }

    pub(crate) fn learning_source_contexts(
        &self,
        root: RunId,
    ) -> Result<Vec<RunContextRecord>, StorageError> {
        self.database
            .lock()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .learning_source_contexts(&root.to_string())
    }

    pub(crate) fn learning_lineage_contexts(
        &self,
        roots: &[String],
    ) -> Result<Vec<RunContextRecord>, StorageError> {
        self.database
            .lock()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .learning_lineage_contexts(roots)
    }

    pub(crate) fn user_question(
        &self,
        id: &str,
    ) -> Result<Option<event_bus::UserQuestion>, StorageError> {
        self.database
            .lock()
            .map_err(|e| StorageError::Io(e.to_string()))?
            .user_question(id)
    }
    pub(crate) fn user_questions(
        &self,
        run: RunId,
    ) -> Result<Vec<event_bus::UserQuestion>, StorageError> {
        self.database
            .lock()
            .map_err(|e| StorageError::Io(e.to_string()))?
            .user_questions_for_run(&run.to_string())
    }

    pub(crate) fn user_question_recipients(&self, id: &str) -> Result<Vec<String>, StorageError> {
        self.database
            .lock()
            .map_err(|e| StorageError::Io(e.to_string()))?
            .user_question_recipients(id)
    }

    pub(crate) fn latest_terminal_named(
        &self,
        name: &str,
    ) -> Result<Option<RunContextRecord>, StorageError> {
        self.database
            .lock()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .latest_terminal_run_context(name)
    }

    /// 同じ DB を使用する writer handle と読み取り接続を束ねる。
    ///
    /// # Errors
    /// DB を開けない場合に storage のエラーを返す。
    pub fn open(config: &StorageConfig, handle: StorageHandle) -> Result<Self, StorageError> {
        let database = Database::open(config)?;
        let mut latest_run_id = None;
        for id in database.persisted_run_ids()? {
            let id = id
                .parse::<RunId>()
                .map_err(|error| StorageError::Serialization(error.to_string()))?;
            latest_run_id = latest_run_id.max(Some(id));
        }
        Ok(Self {
            db_path: config.db_path.clone(),
            handle,
            database: Mutex::new(database),
            latest_run_id,
            restore_gate: Mutex::new(()),
        })
    }

    pub(crate) fn restore_record(
        &self,
        run_id: RunId,
    ) -> Result<Option<RunContextRecord>, StorageError> {
        Ok(self.restore_bundle(run_id)?.map(|bundle| bundle.record))
    }

    pub(crate) fn restore_bundle(
        &self,
        run_id: RunId,
    ) -> Result<Option<storage::thread_index::SnapshotBundle>, StorageError> {
        self.database
            .lock()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .snapshot_bundle(&run_id.to_string())
            .map_err(crate::restore::provenance::storage_error)
    }

    pub(crate) fn restore_provenance(
        &self,
        record: &RunContextRecord,
    ) -> Result<crate::restore::provenance::ConversationProvenance, StorageError> {
        let run = record
            .run_id
            .parse::<RunId>()
            .map_err(|error| StorageError::Serialization(error.to_string()))?;
        Ok(self
            .restore_bundle(run)?
            .filter(|bundle| {
                bundle.record.messages_json == record.messages_json
                    && bundle.record.checkpoints_json == record.checkpoints_json
            })
            .map_or_else(
                crate::restore::provenance::ConversationProvenance::unknown,
                |bundle| crate::restore::provenance::ConversationProvenance::from_bundle(&bundle),
            ))
    }

    pub(crate) fn restore_conversation(
        &self,
        record: &RunContextRecord,
    ) -> Result<crate::restore::RestoredState, crate::RuntimeError> {
        let mut restored = crate::restore::RestoredState::for_conversation(record)?;
        restored.provenance = self.restore_provenance(record).map_err(|error| {
            crate::RuntimeError::RunRestoreFailed {
                run_id: record.run_id.clone(),
                reason: crate::RunRestoreFailure::CorruptContext(error.to_string()),
            }
        })?;
        let saved_messages: Vec<serde_json::Value> = serde_json::from_str(&record.messages_json)
            .expect("conversation restore validated JSON");
        restored
            .provenance
            .generated_since(&restored.messages, saved_messages.len());
        Ok(restored)
    }

    pub(crate) fn restore_parent(
        &self,
        run_id: RunId,
    ) -> Result<Option<Option<String>>, StorageError> {
        self.database
            .lock()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .run_context_parent(&run_id.to_string())
    }

    pub(crate) fn ledger_entries(
        &self,
        run_id: RunId,
    ) -> Result<Vec<RunLedgerEntry>, StorageError> {
        self.database
            .lock()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .run_ledger(&run_id.to_string())
    }
}

impl RunStore {
    pub(crate) fn review_evidence_for_lineage(
        &self,
        run_ids: &[String],
    ) -> Result<Vec<storage::ReviewEvidenceRow>, StorageError> {
        self.database
            .lock()
            .map_err(|e| StorageError::Io(e.to_string()))?
            .review_evidence_for_lineage(run_ids)
    }

    pub(crate) fn review_lineage_ancestors(
        &self,
        run_id: &str,
    ) -> Result<Vec<String>, StorageError> {
        self.database
            .lock()
            .map_err(|e| StorageError::Io(e.to_string()))?
            .review_lineage_ancestors(run_id)
    }

    pub(crate) fn review_lineage_edges(
        &self,
        run_id: &str,
    ) -> Result<Vec<storage::LineageEdge>, StorageError> {
        self.database
            .lock()
            .map_err(|e| StorageError::Io(e.to_string()))?
            .review_lineage_edges(run_id)
    }
}

/// Rebuild only explicitly recorded ancestry, never inferred message roles or summaries.
pub(crate) fn materialize_review_context(
    store: &RunStore,
    run_id: RunId,
) -> Result<crate::escalation_review::ReviewRunContext, StorageError> {
    use crate::escalation_review::{ReviewRunContext, SharedUserRequests};
    let id = run_id.to_string();
    let mut lineage = store.review_lineage_ancestors(&id)?;
    lineage.push(id.clone());
    let rows = store.review_evidence_for_lineage(&lineage)?;
    if lineage.len() == 1 && rows.is_empty() {
        return Err(StorageError::Serialization(
            "review lineage is unavailable for legacy run".into(),
        ));
    }
    let mut roots = Vec::new();
    for ancestor in &lineage {
        if store.review_lineage_edges(ancestor)?.is_empty() {
            roots.push(ancestor.clone());
        }
    }
    if roots.len() != 1 {
        return Err(StorageError::Serialization(
            "review lineage has no unique root".into(),
        ));
    }
    Ok(ReviewRunContext {
        root_run_id: roots.remove(0),
        lineage_run_ids: lineage,
        user_requests: std::sync::Arc::new(Mutex::new(SharedUserRequests::from(
            rows.into_iter()
                .map(request_from_evidence)
                .collect::<Vec<_>>(),
        ))),
        delegation_chain: Vec::new(),
        loaded_skills: Vec::new(),
    })
}

pub(crate) fn request_from_evidence(
    row: storage::ReviewEvidenceRow,
) -> crate::escalation_review::UserRequest {
    use crate::escalation_review::{UserQuestionContext, UserRequest};
    UserRequest {
        target_run_id: row.target_run_id,
        text: row.text,
        in_reply_to: row.in_reply_to_question_id.map(|id| UserQuestionContext {
            id,
            title: row.in_reply_to_title.unwrap_or_default(),
        }),
        // This optional assistant-authored annotation cannot invalidate user evidence.
        reference_context: row
            .reference_context
            .and_then(|json| serde_json::from_str(&json).ok()),
    }
}

/// Replace the contents, not the Arc: all live descendants retain the same fence.
/// The caller serializes materialization and host writes with review_runs.
pub(crate) fn resync_review_context(
    store: &RunStore,
    run: &crate::escalation_review::ReviewRunContext,
    lineage_run_ids: &[String],
) -> Result<(), StorageError> {
    let mut shared = run
        .user_requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let result = (|| {
        let mut lineage = Vec::new();
        for id in lineage_run_ids {
            lineage.extend(store.review_lineage_ancestors(id)?);
            lineage.push(id.clone());
        }
        lineage.sort();
        lineage.dedup();
        store.review_evidence_for_lineage(&lineage)
    })();
    shared.evidence_revision += 1;
    match result {
        Ok(rows) => {
            shared.requests = rows.into_iter().map(request_from_evidence).collect();
            shared.synced = true;
            Ok(())
        }
        Err(error) => {
            shared.synced = false;
            Err(error)
        }
    }
}

pub(crate) fn evidence_row(
    root: &str,
    target: &str,
    kind: &str,
    text: &str,
) -> storage::ReviewEvidenceRow {
    use sha2::{Digest, Sha256};
    storage::ReviewEvidenceRow {
        id: format!(
            "{:x}",
            Sha256::digest(format!("{root}|{target}|{kind}|{text}|"))
        ),
        root_run_id: root.into(),
        target_run_id: target.into(),
        kind: kind.into(),
        text: text.into(),
        in_reply_to_question_id: None,
        in_reply_to_title: None,
        reference_kind: None,
        reference_context: None,
        created_at_ns: review_time_ns(),
    }
}

pub(crate) fn review_time_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod review_tests {
    use super::*;

    #[test]
    fn resync_keeps_shared_arc_and_lineage_on_failure_then_recovers_full_reference() {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("review.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let store = RunStore::open(&config, storage.handle()).unwrap();
        let root = crate::run_ids::RunIds::default().next();
        let mut row = evidence_row(
            &root.to_string(),
            &root.to_string(),
            "host_chat",
            "real reply",
        );
        let reference = crate::escalation_review::ReferenceContext {
            text: "assistant context".into(),
            source_run_id: root.to_string(),
            turn_end: 12,
        };
        row.reference_kind = Some("assistant_context_v1".into());
        row.reference_context = Some(serde_json::to_string(&reference).unwrap());
        store.handle.record_review_evidence(&[row]).unwrap();
        let review = materialize_review_context(&store, root).unwrap();
        let shared = review.user_requests.clone();
        let lineage = review.lineage_run_ids.clone();
        review.mark_unsynced();
        let before = review.stamp();
        resync_review_context(&store, &review, &lineage).unwrap();
        assert!(std::sync::Arc::ptr_eq(&shared, &review.user_requests));
        assert!(review.stamp().synced);
        assert!(review.stamp().revision > before.revision);
        assert_eq!(review.requests().0[0].reference_context, Some(reference));
        store
            .handle
            .record_review_lineage_edge(&[storage::LineageEdge {
                child_run_id: root.to_string(),
                parent_run_id: root.to_string(),
                created_at_ns: 0,
            }])
            .unwrap();
        assert!(resync_review_context(&store, &review, &lineage).is_err());
        assert!(!review.stamp().synced);
        assert_eq!(review.lineage_run_ids, lineage);
        assert!(std::sync::Arc::ptr_eq(&shared, &review.user_requests));
    }

    #[test]
    fn materialize_uses_edges_not_cached_root_or_evidence_kind_and_keeps_bad_reference_text() {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("review.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let store = RunStore::open(&config, storage.handle()).unwrap();
        let root = crate::run_ids::RunIds::default().next();
        let child = crate::run_ids::RunIds::default().next();
        let mut row = evidence_row(
            "untrusted-cache",
            &root.to_string(),
            "unknown-kind",
            "real human text",
        );
        row.reference_context = Some("invalid JSON".into());
        store.handle.record_review_evidence(&[row]).unwrap();
        store
            .handle
            .record_review_lineage_edge(&[storage::LineageEdge {
                child_run_id: child.to_string(),
                parent_run_id: root.to_string(),
                created_at_ns: 0,
            }])
            .unwrap();
        let review = materialize_review_context(&store, child).unwrap();
        assert_eq!(review.root_run_id, root.to_string());
        assert_eq!(review.requests().0[0].text, "real human text");
        assert!(review.requests().0[0].reference_context.is_none());
        assert!(
            materialize_review_context(&store, crate::run_ids::RunIds::default().next()).is_err()
        );
    }
}
