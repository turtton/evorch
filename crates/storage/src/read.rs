//! `Database` の read-only facade を定義します。

use event_bus::{AgentMessage, AgentMessageEvent, DeliveryDisposition, EventKind, UsageBucket};

use crate::entity::{AgentRunRecord, MessageRecord, SessionRecord, TaskRecord};
use crate::projection;
use crate::repo::{agent_run, event, message, metrics, run_context, run_ledger, session, task};
use crate::{
    Database, EventPage, RunContextRecord, RunLedgerEntry, SessionSnapshot, StorageError,
    StoredEvent,
};

/// 永続化済みの AgentMessage 配送です。
#[derive(Debug, Clone, PartialEq)]
pub struct StoredAgentMessage {
    /// 配送されたメッセージ封筒です。
    pub message: AgentMessage,
    /// 配送時に確定した受信側での扱いです。
    pub disposition: DeliveryDisposition,
}

/// Saved identity and state, without reading configuration or conversation JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunContextHeader {
    pub run_id: String,
    pub terminal_phase: String,
    pub updated_at_ns: i64,
}

/// Snapshot identity and phase, without selecting any JSON payload columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunContextMeta {
    pub name: String,
    pub role: String,
    pub parent_run_id: Option<String>,
    pub terminal_phase: String,
    pub updated_at_ns: i64,
}

impl Database {
    pub fn user_question(&self, id: &str) -> Result<Option<event_bus::UserQuestion>, StorageError> {
        crate::repo::user_questions::get(&self.conn, id)
    }
    pub fn user_questions_for_run(
        &self,
        run_id: &str,
    ) -> Result<Vec<event_bus::UserQuestion>, StorageError> {
        crate::repo::user_questions::for_run(&self.conn, run_id)
    }
    pub fn user_question_recipients(&self, id: &str) -> Result<Vec<String>, StorageError> {
        crate::repo::user_questions::recipients(&self.conn, id)
    }
    pub fn pending_user_questions(&self) -> Result<Vec<event_bus::UserQuestion>, StorageError> {
        crate::repo::user_questions::pending(&self.conn)
    }

    /// Latest terminal snapshot or safe checkpoint for an exact run name, ordered by persistence time and numeric ID.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn latest_terminal_run_context(
        &self,
        name: &str,
    ) -> Result<Option<RunContextRecord>, StorageError> {
        use rusqlite::OptionalExtension;
        let id: Option<String> = self.conn.query_row(
            "SELECT run_id FROM run_contexts WHERE name = ?1 AND terminal_phase IN ('Done', 'Error', 'Checkpoint', 'Stopped') \
             AND run_id NOT IN (SELECT run_id FROM run_tombstones) \
             ORDER BY updated_at_ns DESC, CAST(substr(run_id, 5) AS INTEGER) DESC LIMIT 1",
            [name], |row| row.get(0),
        ).optional()?;
        id.map(|id| self.run_context(&id))
            .transpose()
            .map(Option::flatten)
    }

    /// Read only the identity needed to authorize access to a run's context.
    ///
    /// # Errors
    /// Returns an error if the identity column cannot be read.
    pub fn run_context_parent(&self, run_id: &str) -> Result<Option<Option<String>>, StorageError> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                "SELECT parent_run_id FROM run_contexts WHERE run_id = ?1 \
                 AND run_id NOT IN (SELECT run_id FROM run_tombstones)",
                [run_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Evidence for one exact target, ordered by creation time and ID.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn review_evidence_for_target(
        &self,
        run_id: &str,
    ) -> Result<Vec<crate::ReviewEvidenceRow>, StorageError> {
        crate::repo::review_evidence::for_target(&self.conn, run_id)
    }

    /// Evidence matching the cached root, ordered by creation time and ID.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn review_evidence_for_root(
        &self,
        run_id: &str,
    ) -> Result<Vec<crate::ReviewEvidenceRow>, StorageError> {
        crate::repo::review_evidence::for_root(&self.conn, run_id)
    }

    /// Evidence for the exact target set, ordered by creation time and ID without duplicates.
    ///
    /// # Errors
    /// Returns an error if SQLite access, set serialization or row decoding fails.
    pub fn review_evidence_for_lineage(
        &self,
        run_ids: &[String],
    ) -> Result<Vec<crate::ReviewEvidenceRow>, StorageError> {
        crate::repo::review_evidence::for_lineage(&self.conn, run_ids)
    }

    /// Explicit parent edges for a child, ordered by parent ID.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn review_lineage_edges(
        &self,
        run_id: &str,
    ) -> Result<Vec<crate::LineageEdge>, StorageError> {
        crate::repo::review_lineage::for_child(&self.conn, run_id)
    }

    /// All reachable parents in ID order, excluding the requested run itself.
    ///
    /// # Errors
    /// Fails closed on cycles or paths exceeding 64 edges, as well as SQLite or decoding errors.
    pub fn review_lineage_ancestors(&self, run_id: &str) -> Result<Vec<String>, StorageError> {
        crate::repo::review_lineage::ancestors(&self.conn, run_id)
    }

    /// Every run ID reserved by contexts, notes, questions, evidence, assistant references or thread starts.
    ///
    /// # Errors
    /// Returns an error for unreadable rows.
    pub fn persisted_run_ids(&self) -> Result<Vec<String>, StorageError> {
        let mut statement = self
            .conn
            .prepare("SELECT run_id FROM run_contexts UNION SELECT run_id FROM run_ledger UNION SELECT run_id FROM user_questions UNION SELECT run_id FROM user_question_links UNION SELECT target_run_id FROM review_evidence UNION SELECT root_run_id FROM review_evidence UNION SELECT child_run_id FROM review_lineage_edges UNION SELECT parent_run_id FROM review_lineage_edges UNION SELECT source_run_id FROM assistant_references UNION SELECT root_run_id FROM thread_start_states")?;
        let ids = statement.query_map([], |row| row.get::<_, String>(0))?;
        Ok(ids.collect::<Result<_, _>>()?)
    }

    /// Return a run's ledger entries in global sequence order.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn run_ledger(&self, run_id: &str) -> Result<Vec<RunLedgerEntry>, StorageError> {
        run_ledger::list_by_run(&self.conn, run_id)
    }

    /// Return the latest complete note revision, including an explicit empty clear.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn latest_run_ledger(&self, run_id: &str) -> Result<Option<RunLedgerEntry>, StorageError> {
        run_ledger::latest(&self.conn, run_id)
    }

    /// Resolve a durable assistant reference, including its consumed state.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn assistant_reference(
        &self,
        id: &str,
    ) -> Result<Option<crate::AssistantReferenceRow>, StorageError> {
        crate::repo::assistant_reference::get(&self.conn, id)
    }

    /// A thread's start state with its untracked entries.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn thread_start_state(
        &self,
        thread_id: &str,
    ) -> Result<Option<crate::thread_start::ThreadStartState>, StorageError> {
        crate::repo::thread_start::get(&self.conn, thread_id)
    }

    /// Start states captured at or after `since_ns`, newest first, without contents.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn thread_start_states_since(
        &self,
        since_ns: i64,
    ) -> Result<Vec<crate::thread_start::ThreadStartSummary>, StorageError> {
        crate::repo::thread_start::list_since(&self.conn, since_ns)
    }

    /// The evaluation sample pinned for a thread.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn eval_sample(
        &self,
        thread_id: &str,
    ) -> Result<Option<crate::eval_sample::EvalSample>, StorageError> {
        crate::repo::eval_sample::get(&self.conn, thread_id)
    }

    /// All evaluation samples, most recently updated first.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn eval_samples(&self) -> Result<Vec<crate::eval_sample::EvalSample>, StorageError> {
        crate::repo::eval_sample::list(&self.conn)
    }

    /// Return all ledger entries in global sequence order.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn run_ledger_all(&self) -> Result<Vec<RunLedgerEntry>, StorageError> {
        run_ledger::list_all(&self.conn)
    }

    /// Return the latest snapshot for a run, if present.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn run_context(&self, run_id: &str) -> Result<Option<RunContextRecord>, StorageError> {
        run_context::get(&self.conn, run_id)
    }

    /// Read only snapshot metadata; compressed conversation/checkpoints are not read.
    pub fn run_context_meta(&self, run_id: &str) -> Result<Option<RunContextMeta>, StorageError> {
        run_context::meta(&self.conn, run_id)
    }

    /// Read conversation JSON only for a checkpoint. Phase and payload come
    /// from the same SQLite snapshot, even if another connection saves a run.
    pub fn checkpoint_run_messages(&self, run_id: &str) -> Result<Option<String>, StorageError> {
        run_context::checkpoint_messages(&self.conn, run_id)
    }

    /// Saved headers in requested order, once per ID, skipping unsaved runs.
    /// This does not parse or return any configuration or conversation payload.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn run_context_headers(
        &self,
        run_ids: &[String],
    ) -> Result<Vec<RunContextHeader>, StorageError> {
        run_context::headers(&self.conn, run_ids)
    }

    /// Delivery receipts for the given prompt IDs, in request order; IDs
    /// without a receipt are skipped.
    pub fn host_input_receipts(
        &self,
        prompt_ids: &[String],
    ) -> Result<Vec<crate::HostInputReceipt>, StorageError> {
        crate::repo::host_input_receipt::for_ids(&self.conn, prompt_ids)
    }

    /// What each submission recorded (the host's words and the skill names
    /// attached to them), in request order; IDs without a receipt are skipped.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn host_input_origins(
        &self,
        prompt_ids: &[String],
    ) -> Result<Vec<crate::HostInputOrigin>, StorageError> {
        crate::repo::host_input_receipt::origins(&self.conn, prompt_ids)
    }

    /// Evidence that attached skills, one entry per accepted submission in the
    /// order the submissions were accepted, for the exact target set. The same
    /// evidence appears again for each repeat submission.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn review_attachment_acceptances_for_lineage(
        &self,
        run_ids: &[String],
    ) -> Result<Vec<crate::ReviewEvidenceRow>, StorageError> {
        crate::repo::review_evidence::attachment_acceptances(&self.conn, run_ids)
    }

    /// One run's delivery receipts, oldest first.
    pub fn host_input_receipts_for_run(
        &self,
        run_id: &str,
    ) -> Result<Vec<crate::HostInputReceipt>, StorageError> {
        crate::repo::host_input_receipt::for_run(&self.conn, run_id)
    }

    /// Read one consistent, bounded snapshot of a root and its persisted descendants.
    /// Limits fail closed rather than silently omitting potential evidence.
    pub fn learning_source_contexts(
        &self,
        root: &str,
    ) -> Result<Vec<RunContextRecord>, StorageError> {
        self.learning_lineage_contexts(&[root.to_owned()])
    }

    /// Capture several root trees in one bounded read transaction.
    pub fn learning_lineage_contexts(
        &self,
        roots: &[String],
    ) -> Result<Vec<RunContextRecord>, StorageError> {
        const MAX_RUNS: usize = 128;
        const MAX_BYTES: i64 = 16 * 1024 * 1024;
        let transaction = self.conn.unchecked_transaction()?;
        let ids = {
            let mut statement = transaction.prepare(
                "WITH RECURSIVE tree(run_id) AS (SELECT run_id FROM run_contexts WHERE run_id IN (SELECT value FROM json_each(?1)) \
                 AND run_id NOT IN (SELECT run_id FROM run_tombstones) \
                 UNION SELECT child.run_id FROM run_contexts child JOIN tree ON child.parent_run_id = tree.run_id \
                 WHERE child.run_id NOT IN (SELECT run_id FROM run_tombstones)) \
                 SELECT context.run_id, (context.messages_len + context.checkpoints_len + length(CAST(context.config_json AS BLOB))) FROM run_contexts context JOIN tree USING(run_id) \
                 ORDER BY CAST(substr(context.run_id, 5) AS INTEGER) LIMIT ?2",
            )?;
            statement
                .query_map(
                    rusqlite::params![
                        serde_json::to_string(roots)
                            .map_err(|error| StorageError::Serialization(error.to_string()))?,
                        (MAX_RUNS + 1) as i64
                    ],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )?
                .collect::<Result<Vec<_>, _>>()?
        };
        if ids.len() > MAX_RUNS || ids.iter().map(|(_, bytes)| *bytes).sum::<i64>() > MAX_BYTES {
            return Err(StorageError::Serialization(
                "learning source exceeds snapshot limit (128 runs / 16 MiB)".into(),
            ));
        }
        let records = ids
            .into_iter()
            .map(|(id, _)| {
                run_context::get(&transaction, &id)?.ok_or_else(|| {
                    StorageError::Serialization("learning source disappeared".into())
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        transaction.commit()?;
        Ok(records)
    }

    /// 識別子に一致するセッションを返します。
    ///
    /// # Errors
    /// SQLite 操作または保存値の変換に失敗した場合にエラーを返します。
    pub fn session(&self, id: &str) -> Result<Option<SessionRecord>, StorageError> {
        session::get(&self.conn, id)
    }

    /// 親セッションに属する子セッションを作成日時順で返します。
    ///
    /// # Errors
    /// SQLite 操作または保存値の変換に失敗した場合にエラーを返します。
    pub fn sessions_by_parent(&self, parent_id: &str) -> Result<Vec<SessionRecord>, StorageError> {
        session::list_by_parent(&self.conn, parent_id)
    }

    /// 識別子に一致するタスクを返します。
    ///
    /// # Errors
    /// SQLite 操作または保存値の変換に失敗した場合にエラーを返します。
    pub fn task(&self, id: &str) -> Result<Option<TaskRecord>, StorageError> {
        task::get(&self.conn, id)
    }

    /// セッションに属するタスクを作成日時順で返します。
    ///
    /// # Errors
    /// SQLite 操作または保存値の変換に失敗した場合にエラーを返します。
    pub fn tasks_by_session(&self, session_id: &str) -> Result<Vec<TaskRecord>, StorageError> {
        task::list_by_session(&self.conn, session_id)
    }

    /// 識別子に一致するメッセージを返します。
    ///
    /// # Errors
    /// SQLite 操作または保存値の変換に失敗した場合にエラーを返します。
    pub fn message(&self, id: &str) -> Result<Option<MessageRecord>, StorageError> {
        message::get(&self.conn, id)
    }

    /// セッションに属するメッセージを作成日時順で返します。
    ///
    /// # Errors
    /// SQLite 操作または保存値の変換に失敗した場合にエラーを返します。
    pub fn messages_by_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<MessageRecord>, StorageError> {
        message::list_by_session(&self.conn, session_id)
    }

    /// 識別子に一致するエージェント実行記録を返します。
    ///
    /// # Errors
    /// SQLite 操作または保存値の変換に失敗した場合にエラーを返します。
    pub fn agent_run(&self, id: &str) -> Result<Option<AgentRunRecord>, StorageError> {
        agent_run::get(&self.conn, id)
    }

    /// セッションに属するエージェント実行記録を開始日時順で返します。
    ///
    /// # Errors
    /// SQLite 操作または保存値の変換に失敗した場合にエラーを返します。
    pub fn agent_runs_by_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<AgentRunRecord>, StorageError> {
        agent_run::list_by_session(&self.conn, session_id)
    }

    /// セッションに属するイベントを採番順で返します。
    ///
    /// # Errors
    /// SQLite 操作またはイベントの復元に失敗した場合にエラーを返します。
    pub fn events_by_session(&self, session_id: &str) -> Result<Vec<StoredEvent>, StorageError> {
        event::list_by_session(&self.conn, session_id)
    }

    /// 指定セッションの AgentMessage 配送を insertion 順に復元する。
    ///
    /// # Errors
    /// SQLite 操作または保存済み AgentMessage イベントの復元に失敗した場合にエラーを返します。
    pub fn agent_messages_by_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<StoredAgentMessage>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT id, kind, payload FROM events \
             WHERE session_id = ?1 AND kind = 'AgentMessage' \
             AND (run_id IS NULL OR run_id NOT IN (SELECT run_id FROM run_tombstones)) \
             ORDER BY id ASC",
        )?;
        let tombstones = crate::thread_retention::TombstoneSet::load(&self.conn)?;
        let mut rows = statement.query(rusqlite::params![session_id])?;
        let mut messages = Vec::new();

        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let kind: String = row.get(1)?;
            let payload = crate::codec::decode(row.get_ref(2)?)?;
            let event_kind = serde_json::from_str::<EventKind>(&payload).map_err(|error| {
                StorageError::Serialization(format!("event id {id} kind {kind}: {error}"))
            })?;

            // An agent message names runs in its payload, which the column may not.
            if tombstones.rejects(&event_kind).is_some() {
                continue;
            }

            match event_kind {
                EventKind::AgentMessage(AgentMessageEvent::Delivered {
                    message,
                    disposition,
                }) => messages.push(StoredAgentMessage {
                    message,
                    disposition,
                }),
                EventKind::Lifecycle(_)
                | EventKind::Ledger(_)
                | EventKind::Message(_)
                | EventKind::Tool(_)
                | EventKind::Usage(_)
                | EventKind::Provider(_)
                | EventKind::Fault(_)
                | EventKind::Compaction(_)
                | EventKind::Orchestrator(_)
                | EventKind::Diagnostic(_)
                | EventKind::Ownership(_) => {
                    return Err(StorageError::Serialization(format!(
                        "event id {id} kind {kind}: payload event kind does not match kind column"
                    )));
                }
            }
        }

        Ok(messages)
    }

    /// Whether a thread goal (ADR 0028) was ever bound to the thread.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn thread_goal_recorded(&self, thread_id: &str) -> Result<bool, StorageError> {
        event::thread_goal_recorded(&self.conn, thread_id)
    }

    /// 全イベントを採番順で返します。
    ///
    /// # Errors
    /// SQLite 操作またはイベントの復元に失敗した場合にエラーを返します。
    pub fn events_all_ordered(&self) -> Result<Vec<StoredEvent>, StorageError> {
        event::list_all_ordered(&self.conn)
    }

    /// Up to `limit` events with `id > after_id` in ID order, for following the
    /// log written by other processes. The page's `last_id` also advances past
    /// rows that no longer decode.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn events_after(&self, after_id: i64, limit: usize) -> Result<EventPage, StorageError> {
        event::list_after(&self.conn, after_id, limit, None)
    }

    /// [`Self::events_after`] that passes over the rows of `exclude_session`
    /// (a process following the log skips its own) without reading them.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn events_after_excluding(
        &self,
        after_id: i64,
        limit: usize,
        exclude_session: &str,
    ) -> Result<EventPage, StorageError> {
        event::list_after(&self.conn, after_id, limit, Some(exclude_session))
    }

    /// Largest event ID, or 0 for an empty log.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn last_event_id(&self) -> Result<i64, StorageError> {
        event::last_id(&self.conn)
    }

    /// SQLite `data_version`: changes whenever another connection commits, and
    /// is read from shared memory without touching the database file.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn data_version(&self) -> Result<i64, StorageError> {
        self.pragma_i64("data_version")
    }

    /// `EventKind::Orchestrator` events in ID order, such as thread goal and
    /// procedure snapshots, without decoding the rest of the history.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn orchestrator_events_ordered(&self) -> Result<Vec<StoredEvent>, StorageError> {
        event::list_orchestrator_ordered(&self.conn)
    }

    /// Hands the events of the given `(kind, subkind)` pairs to `visit` in ID
    /// order, one at a time. `kind` is the stored category name (`Orchestrator`,
    /// `Message`, `compaction`, ...); a `None` subkind matches every subkind of
    /// the kind. Rows are found through an index, so a selective filter does not
    /// read the rest of the history. Rows written before the event columns existed
    /// are still matched until the backfill converts them, at the cost of
    /// decoding every such row of the kind. Events that no longer decode are
    /// skipped, as in [`Self::events_all_ordered`].
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn for_each_event_matching(
        &self,
        filters: &[(&str, Option<&str>)],
        visit: impl FnMut(StoredEvent),
    ) -> Result<(), StorageError> {
        event::select::for_each_matching(&self.conn, filters, visit)
    }

    /// Hands every event that concerns `run_ids` to `visit` in ID order: the
    /// events attributed to a run by `Event::primary_run_id`, agent messages
    /// sent by one, and approvals and denials whose call ID (`<run>:<call>`)
    /// belongs to one. Approvals with an unscoped call ID are not attributed.
    /// Unconverted rows are attributed from their payload.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn for_each_event_for_runs(
        &self,
        run_ids: &[String],
        visit: impl FnMut(StoredEvent),
    ) -> Result<(), StorageError> {
        event::select::for_each_for_runs(&self.conn, run_ids, visit)
    }

    /// `run_ids` and every run delegated from them, sorted.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn runs_with_descendants(&self, run_ids: &[String]) -> Result<Vec<String>, StorageError> {
        run_context::with_descendants(&self.conn, run_ids)
    }

    /// Usage ledger rows of `run_ids` in time order, joined with their owner.
    /// Rows already rolled up into daily totals are not included.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn usage_requests_for_runs(
        &self,
        run_ids: &[String],
    ) -> Result<Vec<crate::usage::UsageRequestRow>, StorageError> {
        crate::repo::usage::list_requests_for_runs(&self.conn, run_ids)
    }

    /// Up to `limit` events at or after `from_ns` concerning `run_ids`, oldest
    /// first, and whether more matched. Which events concern a run is defined
    /// by [`Self::for_each_event_for_runs`].
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn events_for_runs(
        &self,
        run_ids: &[String],
        from_ns: i64,
        limit: usize,
    ) -> Result<(Vec<StoredEvent>, bool), StorageError> {
        event::select::list_for_runs(&self.conn, run_ids, from_ns, limit)
    }

    /// [`Self::events_for_runs`] over events with `after_id < id <= through_id`.
    /// Read a long history in pages by passing the last ID of a page, while the
    /// answer says more matched, as the next `after_id`.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn events_for_runs_between(
        &self,
        run_ids: &[String],
        after_id: i64,
        through_id: i64,
        limit: usize,
    ) -> Result<(Vec<StoredEvent>, bool), StorageError> {
        event::select::list_for_runs_between(&self.conn, run_ids, after_id, through_id, limit)
    }

    /// Hands every event to `visit` in ID order, one at a time, so that a
    /// replay never holds the whole history in memory. Events that no longer
    /// decode are skipped, as in [`Self::events_all_ordered`].
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn for_each_event_ordered(
        &self,
        visit: impl FnMut(StoredEvent),
    ) -> Result<(), StorageError> {
        event::for_each_ordered(&self.conn, visit)
    }

    /// inclusive な window_start 範囲の usage バケットをキー順で返します。
    ///
    /// # Errors
    /// SQLite 操作または保存値の変換に失敗した場合にエラーを返します。
    pub fn metrics_range(
        &self,
        from_window_start: u64,
        to_window_start: u64,
    ) -> Result<Vec<UsageBucket>, StorageError> {
        metrics::list_range(&self.conn, from_window_start, to_window_start)
    }

    /// `[from_ns, to_ns)` の usage ledger 行を時刻順に、run の所属と結合して返します。
    ///
    /// # Errors
    /// SQLite 操作または保存値の変換に失敗した場合にエラーを返します。
    pub fn usage_requests_between(
        &self,
        from_ns: i64,
        to_ns: i64,
    ) -> Result<Vec<crate::usage::UsageRequestRow>, StorageError> {
        crate::repo::usage::list_requests(&self.conn, from_ns, to_ns)
    }

    /// inclusive なローカル日付範囲 (`YYYY-MM-DD`) の usage ledger 行を時刻順に返します。
    ///
    /// # Errors
    /// SQLite 操作または保存値の変換に失敗した場合にエラーを返します。
    pub fn usage_requests_in_days(
        &self,
        from_day: &str,
        to_day: &str,
    ) -> Result<Vec<crate::usage::UsageRequestRow>, StorageError> {
        crate::repo::usage::list_requests_in_days(&self.conn, from_day, to_day)
    }

    /// usage ledger と同じ時計での今日のローカル日付と、その開始・終了時刻を返します。
    ///
    /// # Errors
    /// SQLite 操作に失敗した場合にエラーを返します。
    pub fn usage_local_clock(&self) -> Result<crate::usage::LocalClock, StorageError> {
        crate::repo::usage::local_clock(&self.conn)
    }

    /// usage ledger と同じ時計での今日のローカル日付 (`YYYY-MM-DD`) を返します。
    ///
    /// # Errors
    /// SQLite 操作に失敗した場合にエラーを返します。
    pub fn usage_local_today(&self) -> Result<String, StorageError> {
        crate::repo::usage::local_today(&self.conn)
    }

    /// 保持期間を過ぎて日次集計へ畳み込まれた usage を、inclusive な日付範囲
    /// (`YYYY-MM-DD`、ローカル日付) で返します。
    ///
    /// # Errors
    /// SQLite 操作または保存値の変換に失敗した場合にエラーを返します。
    pub fn usage_daily_between(
        &self,
        from_day: &str,
        to_day: &str,
    ) -> Result<Vec<crate::usage::UsageDailyRow>, StorageError> {
        crate::repo::usage::list_daily(&self.conn, from_day, to_day)
    }

    /// 指定セッションのイベントを採番順で畳み込み、復元状態を返します。
    ///
    /// # Errors
    /// SQLite 操作またはイベントの復元に失敗した場合にエラーを返します。
    pub fn restore_session(&self, id: &str) -> Result<Option<SessionSnapshot>, StorageError> {
        projection::restore_session(&self.conn, id)
    }

    /// 全イベントを採番順で畳み込み、セッション ID 順の復元状態を返します。
    ///
    /// # Errors
    /// SQLite 操作またはイベントの復元に失敗した場合にエラーを返します。
    pub fn restore_sessions(&self) -> Result<Vec<SessionSnapshot>, StorageError> {
        projection::restore_sessions(&self.conn)
    }
}

/// ADR 0041 Phase B reads. Each call reads in one transaction.
impl Database {
    /// The run's snapshot, provenance sidecar, revisions and latest note.
    ///
    /// # Errors
    /// Returns an error for SQLite failure.
    pub fn snapshot_bundle(
        &self,
        run_id: &str,
    ) -> Result<Option<crate::thread_index::SnapshotBundle>, crate::thread_index::SnapshotApiError>
    {
        use crate::thread_index::SnapshotRepository as _;
        crate::repo::thread_index::SqliteSnapshotStore::new(&self.conn).load_snapshot_bundle(run_id)
    }

    /// Validates a guarded locator against the canonical source. Authorization
    /// must already have been checked by the caller.
    ///
    /// # Errors
    /// Returns an error for SQLite failure.
    pub fn resolve_guarded_reference(
        &self,
        reference: &crate::thread_index::GuardedReference,
    ) -> Result<crate::thread_index::GuardedResolution, crate::thread_index::SnapshotApiError> {
        use crate::thread_index::SnapshotRepository as _;
        crate::repo::thread_index::SqliteSnapshotStore::new(&self.conn)
            .resolve_guarded_reference(reference)
    }

    /// The index state of one run, if it has been seen.
    ///
    /// # Errors
    /// Returns an error for SQLite failure.
    pub fn thread_index_run_status(
        &self,
        run_id: &str,
    ) -> Result<Option<crate::thread_index::IndexRunStatus>, StorageError> {
        crate::repo::thread_index::index_run_status(&self.conn, run_id)
    }
}
