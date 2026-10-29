//! 専用スレッド上の単一 SQLite writer を管理します。

use std::sync::Arc;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread::JoinHandle;

use event_bus::{Event, UsageBucket, UsageSink};

use crate::db::{file_sizes, temp_files_bytes};
use crate::entity::SecretGuard;
use crate::{
    CatalogUpdateRecord, Database, DiagnosticCleanupScope, DiagnosticCleanupSummary,
    ReconcileSummary, StorageConfig, StorageError,
};

mod cleanup;
mod index;
use crate::thread_index::{
    IndexCapacityStatus, IndexLease, IndexOwner, IndexSettingsReceipt, IndexStepResult,
    LeaseResult, ReindexRequest, RequestFailure, RequestStatus, RequestSubmission,
    ThreadIndexSettings,
};
pub use index::{THREAD_INDEX_LEASE_NS, ThreadIndexCounters, ThreadIndexJobStatus};
#[cfg(test)]
mod manual_cleanup_tests;
mod state;
#[cfg(test)]
mod stream_tests;

use state::{StartState, log_size_state, log_temp_state, run_writer_with_index, temp_exceeded};

type ReplyTx<T = ()> = mpsc::Sender<Result<T, StorageError>>;
type SnapshotReplyTx<T = crate::thread_index::SnapshotReceipt> =
    mpsc::Sender<Result<T, crate::thread_index::SnapshotApiError>>;
type ReconcileReplyTx = mpsc::Sender<Result<ReconcileSummary, StorageError>>;

// AgentRunStarted 追加で Event が大型化。Box 化は append hot path に alloc を
// 増やすためサイズを許容する。
#[allow(clippy::large_enum_variant)]
enum Command {
    Usage(Vec<UsageBucket>),
    RecordUsageRequests(Vec<crate::usage::UsageRequestRecord>, ReplyTx),
    AttributeUsageRuns(Vec<crate::usage::RunAttribution>),
    UpsertThreads(
        crate::threads::ThreadWriter,
        Vec<crate::threads::ThreadUpsert>,
        Option<ReplyTx>,
    ),
    AppendEvent(Option<String>, Event, ReplyTx),
    AppendFencedEvent(Option<String>, Event, event_bus::MutationValidator, ReplyTx),
    AppendStreamEvent(String, Event, Option<event_bus::MutationValidator>, ReplyTx),
    RecordCatalogUpdate(CatalogUpdateRecord, ReplyTx),
    Memory(crate::repo::memory::Mutation, ReplyTx),
    RecordImprovementCandidate(
        String,
        crate::improvement::NewImprovementCandidate,
        crate::improvement::ImprovementWritePolicy,
        ReplyTx<crate::improvement::ImprovementRecordOutcome>,
    ),
    SetImprovementStatus(String, crate::improvement::ImprovementStatus, ReplyTx<bool>),
    AttachImprovementDraft(String, String, ReplyTx<bool>),
    TaskQueue(crate::task_queue::Mutation, ReplyTx),
    AppendRunLedger(String, String, mpsc::Sender<Result<u64, StorageError>>),
    LatestRunLedger(String, ReplyTx<Option<crate::RunLedgerEntry>>),
    SaveAssistantReference(crate::AssistantReferenceRow, ReplyTx),
    AssistantReference(String, ReplyTx<Option<crate::AssistantReferenceRow>>),
    ClaimLearning(String, Vec<String>, bool, u32, ReplyTx<bool>),
    SaveThreadStartState(crate::thread_start::ThreadStartState, ReplyTx<bool>),
    SaveEvalSample(crate::eval_sample::EvalSample, ReplyTx),
    DeleteEvalSample(String, ReplyTx<bool>),
    CreateUserQuestion(event_bus::UserQuestion, ReplyTx),
    BindUserQuestions(String, String, Vec<String>, ReplyTx),
    AnswerUserQuestion(
        String,
        String,
        event_bus::AnswerOrigin,
        crate::AnswerMode,
        ReplyTx,
    ),
    AnswerUserQuestionWithEvidence(
        String,
        String,
        event_bus::AnswerOrigin,
        Vec<crate::ReviewEvidenceRow>,
        crate::AnswerMode,
        ReplyTx,
    ),
    UpsertRunContext(crate::RunContextRecord, ReplyTx<crate::RunContextReceipt>),
    AppendIndexedNote(String, String, ReplyTx<crate::thread_index::NoteReceipt>),
    SaveSnapshotBundle(Box<crate::thread_index::SnapshotWrite>, SnapshotReplyTx),
    SaveSnapshotBundleWithEvidence(
        Box<crate::thread_index::SnapshotWrite>,
        Vec<crate::ReviewEvidenceRow>,
        Vec<crate::LineageEdge>,
        Option<String>,
        crate::HostInputWrite,
        SnapshotReplyTx<(
            crate::thread_index::SnapshotReceipt,
            Vec<crate::HostInputTransition>,
        )>,
    ),
    RecordReviewEvidenceWithEdges(
        Vec<crate::ReviewEvidenceRow>,
        Vec<crate::LineageEdge>,
        Option<String>,
        ReplyTx,
    ),
    RecordReviewLineageEdge(Vec<crate::LineageEdge>, ReplyTx),
    UpsertRunContextWithEvidence(
        crate::RunContextRecord,
        Vec<crate::ReviewEvidenceRow>,
        Vec<crate::LineageEdge>,
        Option<String>,
        crate::HostInputWrite,
        ReplyTx<Vec<crate::HostInputTransition>>,
    ),
    UpsertRunContextWithReceipts(
        crate::RunContextRecord,
        crate::HostInputWrite,
        ReplyTx<(crate::RunContextReceipt, Vec<crate::HostInputTransition>)>,
    ),
    AppendHostInputAccepted(
        Vec<crate::ReviewEvidenceRow>,
        Vec<crate::LineageEdge>,
        Option<String>,
        crate::HostInputReceipt,
        ReplyTx,
    ),
    TransitionHostInputs(
        Vec<crate::HostInputTransition>,
        ReplyTx<Vec<crate::HostInputTransition>>,
    ),
    UpdateHostInputReceipts(String, String, ReplyTx<Vec<String>>),
    HostInputReceipts(Vec<String>, ReplyTx<Vec<crate::HostInputReceipt>>),
    HostInputReceiptsForRun(String, ReplyTx<Vec<crate::HostInputReceipt>>),
    CompareAndSwapRunContextRestoreState(String, String, String, bool, ReplyTx<bool>),
    InvalidateRunContext(String, ReplyTx),
    Reconcile(ReconcileReplyTx),
    FlushUsage(ReplyTx),
    Checkpoint(ReplyTx),
    Index(index::IndexCommand),
    Statistics(ReplyTx<StorageStatistics>),
    DiagnosticCleanupStatus(DiagnosticCleanupScope, ReplyTx<cleanup::CleanupStatus>),
    CleanupDiagnosticBatch(
        DiagnosticCleanupScope,
        Option<crate::repo::event::diagnostic::Cursor>,
        crate::repo::event::diagnostic::Cursor,
        ReplyTx<(
            DiagnosticCleanupSummary,
            Option<crate::repo::event::diagnostic::Cursor>,
        )>,
    ),
    ReclaimDiagnosticSpace(u64, ReplyTx<cleanup::ReclaimStep>),
    Shutdown,
}

/// writer のメモリ内統計。取得してもDB・イベント・ログへの書き込みは発生しません。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct StorageStatistics {
    pub events_written: u64,
    pub event_payload_bytes: u64,
    pub event_write_failures: u64,
    pub append_total_micros: u64,
    pub max_append_micros: u64,
    pub usage_flushes: u64,
    pub usage_buckets_written: u64,
    pub maintenance_runs: u64,
}

/// single-writer スレッドの所有権と終了処理を保持します。
pub struct Storage(StorageHandle, Option<JoinHandle<()>>);

impl Storage {
    /// ファイル DB を開き、専用 writer スレッドを開始します。
    pub fn open(config: StorageConfig) -> Result<Self, StorageError> {
        let lease = crate::maintenance::offline::writer_lease(&config.db_path)?;
        let database = Database::open(&config)?;
        let initial_size = file_sizes(&config.db_path)?.total();
        let writes_suspended = initial_size >= config.hard_limits.max_db_bytes;
        log_size_state(initial_size, &config.hard_limits, writes_suspended);
        let soft_warned = !writes_suspended
            && initial_size as f64
                >= config.hard_limits.max_db_bytes as f64 * config.hard_limits.soft_warn_ratio;
        let temp_bytes = temp_files_bytes(&config.db_path)?;
        let temp_warned = temp_exceeded(temp_bytes, config.temp_warn_bytes);
        log_temp_state(temp_bytes, config.temp_warn_bytes, temp_warned);
        let (tx, rx) = mpsc::sync_channel(config.channel_capacity);
        let max_event_bytes = config.hard_limits.max_event_bytes;
        let reader_config = Arc::new(config.clone());
        // Settings are applied (and versions recorded) before any command runs.
        let index = index::IndexJob::new(&database.conn, &config, writes_suspended);
        let writer = std::thread::Builder::new()
            .name("storage-writer".into())
            .spawn(move || {
                let _lease = lease;
                run_writer_with_index(
                    database.conn,
                    rx,
                    config,
                    index,
                    StartState {
                        writes_suspended,
                        soft_warned,
                        suspend_logged: writes_suspended,
                        temp_warned,
                    },
                )
            })
            .map_err(|error| StorageError::Io(error.to_string()))?;
        Ok(Self(
            StorageHandle(tx, max_event_bytes, reader_config),
            Some(writer),
        ))
    }

    /// 複数スレッドから共有可能な writer handle を返します。
    pub fn handle(&self) -> StorageHandle {
        self.0.clone()
    }

    /// 保留中の書き込みを完了して writer スレッドを終了します。
    pub fn close(self) {}

    fn shutdown(&mut self) {
        if let Some(writer) = self.1.take() {
            let _ = self.0.0.send(Command::Shutdown);
            let _ = writer.join();
        }
    }
}

impl Drop for Storage {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// single-writer へ同期要求または lossy usage を送る共有 handle です。
#[derive(Debug, Clone)]
pub struct StorageHandle(SyncSender<Command>, u64, Arc<StorageConfig>);

impl StorageHandle {
    /// Serialized payload limit, retained in memory without contacting the writer.
    pub fn max_event_bytes(&self) -> u64 {
        self.1
    }

    /// Persist a bounded clarification before publishing it to a user.
    pub fn create_user_question(
        &self,
        question: &event_bus::UserQuestion,
    ) -> Result<(), StorageError> {
        self.request(|reply| Command::CreateUserQuestion(question.clone(), reply))
    }
    /// Link explicitly selected questions from a prior run to its continuation.
    pub fn bind_user_questions(
        &self,
        source: &str,
        target: &str,
        ids: &[String],
    ) -> Result<(), StorageError> {
        self.request(|reply| {
            Command::BindUserQuestions(source.into(), target.into(), ids.to_vec(), reply)
        })
    }
    /// First answer wins; retrying the same answer is idempotent.
    pub fn answer_user_question(
        &self,
        id: &str,
        answer: &str,
        origin: &event_bus::AnswerOrigin,
    ) -> Result<(), StorageError> {
        self.answer_user_question_with_mode(id, answer, origin, crate::AnswerMode::Idempotent)
    }

    /// Like [`Self::answer_user_question`], with the caller's choice of what an earlier
    /// answer means; see [`crate::AnswerMode`].
    pub fn answer_user_question_with_mode(
        &self,
        id: &str,
        answer: &str,
        origin: &event_bus::AnswerOrigin,
        mode: crate::AnswerMode,
    ) -> Result<(), StorageError> {
        self.request(|reply| {
            Command::AnswerUserQuestion(id.into(), answer.into(), origin.clone(), mode, reply)
        })
    }

    /// Atomically persists a host answer and all recipient evidence, first answer wins.
    pub fn answer_user_question_with_evidence(
        &self,
        id: &str,
        answer: &str,
        origin: &event_bus::AnswerOrigin,
        evidence: &[crate::ReviewEvidenceRow],
    ) -> Result<(), StorageError> {
        self.answer_user_question_with_evidence_and_mode(
            id,
            answer,
            origin,
            evidence,
            crate::AnswerMode::Idempotent,
        )
    }

    /// Like [`Self::answer_user_question_with_evidence`], with the caller's choice of
    /// what an earlier answer means; see [`crate::AnswerMode`]. No evidence is stored
    /// when the question was already answered in `FirstOnly` mode.
    pub fn answer_user_question_with_evidence_and_mode(
        &self,
        id: &str,
        answer: &str,
        origin: &event_bus::AnswerOrigin,
        evidence: &[crate::ReviewEvidenceRow],
        mode: crate::AnswerMode,
    ) -> Result<(), StorageError> {
        self.request(|reply| {
            Command::AnswerUserQuestionWithEvidence(
                id.into(),
                answer.into(),
                origin.clone(),
                evidence.to_vec(),
                mode,
                reply,
            )
        })
    }

    /// Appends to a cross-session stream, retaining daily, event and database limits.
    pub fn append_stream_event(
        &self,
        stream_id: &str,
        event: &Event,
        validator: Option<event_bus::MutationValidator>,
    ) -> Result<(), StorageError> {
        self.request(|reply| {
            Command::AppendStreamEvent(stream_id.into(), event.clone(), validator, reply)
        })
    }

    /// Append a complete, guarded note revision of up to 8192 bytes; empty clears it.
    ///
    /// # Errors
    /// Returns an error for invalid bodies, suspended writes, SQLite failure or a closed writer.
    pub fn append_run_ledger(&self, run_id: &str, body: &str) -> Result<u64, StorageError> {
        let (reply, result) = mpsc::channel();
        self.0
            .send(Command::AppendRunLedger(run_id.into(), body.into(), reply))
            .map_err(|_| StorageError::WriterClosed)?;
        result.recv().map_err(|_| StorageError::WriterClosed)?
    }

    /// Read the latest complete note revision after all earlier writer requests.
    ///
    /// # Errors
    /// Returns an error for SQLite failure or a closed writer.
    pub fn latest_run_ledger(
        &self,
        run_id: &str,
    ) -> Result<Option<crate::RunLedgerEntry>, StorageError> {
        self.request(|reply| Command::LatestRunLedger(run_id.into(), reply))
    }

    /// Save an immutable assistant reference; identical retries preserve consumption.
    ///
    /// # Errors
    /// Rejects conflicting IDs, invalid text, suspended writes or persistence failure.
    pub fn save_assistant_reference(
        &self,
        row: &crate::AssistantReferenceRow,
    ) -> Result<(), StorageError> {
        self.request(|reply| Command::SaveAssistantReference(row.clone(), reply))
    }

    /// Resolve a durable assistant reference, including its consumed state.
    ///
    /// # Errors
    /// Returns an error for SQLite failure or a closed writer.
    pub fn assistant_reference(
        &self,
        id: &str,
    ) -> Result<Option<crate::AssistantReferenceRow>, StorageError> {
        self.request(|reply| Command::AssistantReference(id.into(), reply))
    }

    /// Atomically admit a pipeline once, with UTC daily quota and source deduplication.
    /// Goal pipelines may reuse roots of earlier goals, but never roots already learned
    /// through the legacy run-completion path. Claims are not retried after a crash.
    ///
    /// # Errors
    /// Returns an error for suspended writes, SQLite failure or a closed writer.
    pub fn claim_learning(
        &self,
        task: &str,
        roots: &[String],
        goal: bool,
        daily_limit: u32,
    ) -> Result<bool, StorageError> {
        self.request(|reply| {
            Command::ClaimLearning(task.into(), roots.to_vec(), goal, daily_limit, reply)
        })
    }

    /// Record a thread's start state; the first state of a thread wins and later
    /// saves return `false`. Secret-bearing content is stored as not replayable.
    ///
    /// # Errors
    /// Rejects content over the size limit, invalid paths, suspended writes or
    /// persistence failure.
    pub fn save_thread_start_state(
        &self,
        state: crate::thread_start::ThreadStartState,
    ) -> Result<bool, StorageError> {
        self.request(|reply| Command::SaveThreadStartState(state, reply))
    }

    /// Pin a thread as an evaluation sample, or replace the problem and focus
    /// roles of its existing sample.
    ///
    /// # Errors
    /// Rejects threads without a replayable start state, an empty or
    /// secret-bearing problem, suspended writes or persistence failure.
    pub fn save_eval_sample(
        &self,
        sample: crate::eval_sample::EvalSample,
    ) -> Result<(), StorageError> {
        self.request(|reply| Command::SaveEvalSample(sample, reply))
    }

    /// Unpin a thread's evaluation sample; returns whether one existed.
    ///
    /// # Errors
    /// Returns an error for suspended writes, SQLite failure or a closed writer.
    pub fn delete_eval_sample(&self, thread_id: &str) -> Result<bool, StorageError> {
        self.request(|reply| Command::DeleteEvalSample(thread_id.into(), reply))
    }

    /// Replace the stored snapshot for a run; the last write wins. The run's
    /// thread binding is updated in the same transaction.
    ///
    /// # Errors
    /// Returns an error for suspended writes, SQLite failure or a closed writer.
    pub fn upsert_run_context(&self, record: &crate::RunContextRecord) -> Result<(), StorageError> {
        self.upsert_run_context_receipt(record).map(drop)
    }

    /// Persist and return the identity of redacted bytes after the SQLite commit.
    /// The receipt does not fence later writes.
    ///
    /// # Errors
    /// Returns an error for suspended writes, SQLite failure or a closed writer.
    pub fn upsert_run_context_receipt(
        &self,
        record: &crate::RunContextRecord,
    ) -> Result<crate::RunContextReceipt, StorageError> {
        self.request(|reply| Command::UpsertRunContext(record.clone(), reply))
    }

    /// Append a note revision (empty clears) and advance the run's index source
    /// in the same transaction; the receipt is returned after commit.
    ///
    /// # Errors
    /// Returns an error for invalid bodies, suspended writes, SQLite failure or a closed writer.
    pub fn append_indexed_note(
        &self,
        run_id: &str,
        body: &str,
    ) -> Result<crate::thread_index::NoteReceipt, StorageError> {
        self.request(|reply| Command::AppendIndexedNote(run_id.into(), body.into(), reply))
    }

    /// Atomically save a redacted snapshot with its binding, provenance sidecar,
    /// source revision and index state (ADR 0041 R1). The receipt exists only
    /// after commit; on error nothing was saved.
    ///
    /// # Errors
    /// Rejects revision conflicts, invalid JSON, suspended writes, SQLite failure or a closed writer.
    pub fn save_snapshot_bundle(
        &self,
        write: &crate::thread_index::SnapshotWrite,
    ) -> Result<crate::thread_index::SnapshotReceipt, crate::thread_index::SnapshotApiError> {
        self.snapshot_request(|reply| Command::SaveSnapshotBundle(Box::new(write.clone()), reply))
    }

    /// [`Self::save_snapshot_bundle`] in the same transaction as evidence,
    /// lineage and an optional one-time reference consumption.
    ///
    /// # Errors
    /// Rejects consumed references, invalid snapshots or evidence, suspended writes or persistence failure.
    pub fn save_snapshot_bundle_with_evidence_and_reference_consumption(
        &self,
        write: &crate::thread_index::SnapshotWrite,
        rows: &[crate::ReviewEvidenceRow],
        edges: &[crate::LineageEdge],
        consumed_reference_id: Option<&str>,
    ) -> Result<crate::thread_index::SnapshotReceipt, crate::thread_index::SnapshotApiError> {
        self.save_snapshot_bundle_with_evidence_and_host_inputs(
            write,
            rows,
            edges,
            consumed_reference_id,
            &crate::HostInputWrite::default(),
        )
        .map(|(receipt, _)| receipt)
    }

    /// Save provenance and host-input delivery receipts in the same transaction.
    ///
    /// # Errors
    /// Rejects invalid snapshots or receipts, suspended writes or persistence failure.
    pub fn save_snapshot_bundle_with_host_inputs(
        &self,
        write: &crate::thread_index::SnapshotWrite,
        host_inputs: &crate::HostInputWrite,
    ) -> Result<
        (
            crate::thread_index::SnapshotReceipt,
            Vec<crate::HostInputTransition>,
        ),
        crate::thread_index::SnapshotApiError,
    > {
        self.save_snapshot_bundle_with_evidence_and_host_inputs(write, &[], &[], None, host_inputs)
    }

    /// Atomically save provenance, evidence, lineage, reference consumption and receipts.
    ///
    /// # Errors
    /// Rejects consumed references, invalid snapshots or receipts, suspended writes or persistence failure.
    pub fn save_snapshot_bundle_with_evidence_and_host_inputs(
        &self,
        write: &crate::thread_index::SnapshotWrite,
        rows: &[crate::ReviewEvidenceRow],
        edges: &[crate::LineageEdge],
        consumed_reference_id: Option<&str>,
        host_inputs: &crate::HostInputWrite,
    ) -> Result<
        (
            crate::thread_index::SnapshotReceipt,
            Vec<crate::HostInputTransition>,
        ),
        crate::thread_index::SnapshotApiError,
    > {
        self.snapshot_request(|reply| {
            Command::SaveSnapshotBundleWithEvidence(
                Box::new(write.clone()),
                rows.to_vec(),
                edges.to_vec(),
                consumed_reference_id.map(String::from),
                host_inputs.clone(),
                reply,
            )
        })
    }

    fn snapshot_request<T>(
        &self,
        command: impl FnOnce(SnapshotReplyTx<T>) -> Command,
    ) -> Result<T, crate::thread_index::SnapshotApiError> {
        let closed = || crate::thread_index::SnapshotApiError::Storage(StorageError::WriterClosed);
        let (reply, result) = mpsc::channel();
        self.0.send(command(reply)).map_err(|_| closed())?;
        result.recv().map_err(|_| closed())?
    }

    /// Persist an idempotent evidence batch without truncating or redacting user text.
    ///
    /// # Errors
    /// Rejects oversized or secret-bearing text, suspended writes, SQLite failure or a closed writer.
    pub fn record_review_evidence(
        &self,
        rows: &[crate::ReviewEvidenceRow],
    ) -> Result<(), StorageError> {
        self.append_review_evidence_with_reference_consumption(rows, &[], None)
    }

    /// Atomically persist evidence and lineage without requiring a run snapshot.
    ///
    /// # Errors
    /// Rejects invalid evidence, suspended writes, SQLite failure or a closed writer.
    pub fn record_review_evidence_with_edges(
        &self,
        rows: &[crate::ReviewEvidenceRow],
        edges: &[crate::LineageEdge],
    ) -> Result<(), StorageError> {
        self.append_review_evidence_with_reference_consumption(rows, edges, None)
    }

    /// Atomically append evidence and lineage, consuming an optional approval once.
    ///
    /// # Errors
    /// Rejects missing or consumed references, invalid evidence, suspended writes or persistence failure.
    pub fn append_review_evidence_with_reference_consumption(
        &self,
        rows: &[crate::ReviewEvidenceRow],
        edges: &[crate::LineageEdge],
        consumed_reference_id: Option<&str>,
    ) -> Result<(), StorageError> {
        self.request(|reply| {
            Command::RecordReviewEvidenceWithEdges(
                rows.to_vec(),
                edges.to_vec(),
                consumed_reference_id.map(String::from),
                reply,
            )
        })
    }

    /// Persist explicit lineage edges as one idempotent batch.
    ///
    /// # Errors
    /// Returns an error for suspended writes, SQLite failure or a closed writer.
    pub fn record_review_lineage_edge(
        &self,
        edges: &[crate::LineageEdge],
    ) -> Result<(), StorageError> {
        self.request(|reply| Command::RecordReviewLineageEdge(edges.to_vec(), reply))
    }

    /// Atomically replace a snapshot and persist its evidence and lineage edges.
    ///
    /// # Errors
    /// Rejects invalid snapshots or evidence, suspended writes, SQLite failure or a closed writer.
    pub fn upsert_run_context_with_evidence(
        &self,
        record: &crate::RunContextRecord,
        rows: &[crate::ReviewEvidenceRow],
        edges: &[crate::LineageEdge],
    ) -> Result<(), StorageError> {
        self.upsert_run_context_with_evidence_and_reference_consumption(record, rows, edges, None)
    }

    /// Atomically replace context and append evidence and lineage, consuming an approval once.
    ///
    /// # Errors
    /// Rejects missing or consumed references, invalid snapshots or evidence, suspended writes or persistence failure.
    pub fn upsert_run_context_with_evidence_and_reference_consumption(
        &self,
        record: &crate::RunContextRecord,
        rows: &[crate::ReviewEvidenceRow],
        edges: &[crate::LineageEdge],
        consumed_reference_id: Option<&str>,
    ) -> Result<(), StorageError> {
        self.upsert_run_context_with_evidence_and_host_inputs(
            record,
            rows,
            edges,
            consumed_reference_id,
            &crate::HostInputWrite::default(),
        )
        .map(drop)
    }

    /// [`Self::upsert_run_context_with_evidence_and_reference_consumption`] that
    /// also applies receipt changes, returning the transitions that took effect.
    ///
    /// # Errors
    /// Rejects missing or consumed references, invalid snapshots, evidence or
    /// receipts, a duplicate prompt ID, suspended writes or persistence failure.
    pub fn upsert_run_context_with_evidence_and_host_inputs(
        &self,
        record: &crate::RunContextRecord,
        rows: &[crate::ReviewEvidenceRow],
        edges: &[crate::LineageEdge],
        consumed_reference_id: Option<&str>,
        host_inputs: &crate::HostInputWrite,
    ) -> Result<Vec<crate::HostInputTransition>, StorageError> {
        self.request(|reply| {
            Command::UpsertRunContextWithEvidence(
                record.clone(),
                rows.to_vec(),
                edges.to_vec(),
                consumed_reference_id.map(String::from),
                host_inputs.clone(),
                reply,
            )
        })
    }

    /// Replace a snapshot and apply receipt changes in one transaction.
    /// Returns the snapshot identity and the transitions that took effect.
    ///
    /// # Errors
    /// Rejects invalid snapshots or receipts, suspended writes, SQLite failure
    /// or a closed writer; nothing is written then.
    pub fn upsert_run_context_with_receipts(
        &self,
        record: &crate::RunContextRecord,
        host_inputs: &crate::HostInputWrite,
    ) -> Result<(crate::RunContextReceipt, Vec<crate::HostInputTransition>), StorageError> {
        self.request(|reply| {
            Command::UpsertRunContextWithReceipts(record.clone(), host_inputs.clone(), reply)
        })
    }

    /// Record that the runtime accepted a host submission: its evidence and
    /// lineage, the approval it consumes and its receipt commit together.
    ///
    /// # Errors
    /// Rejects missing or consumed references, oversized or secret-bearing
    /// text, a duplicate prompt ID, suspended writes or persistence failure.
    pub fn append_host_input_accepted(
        &self,
        rows: &[crate::ReviewEvidenceRow],
        edges: &[crate::LineageEdge],
        consumed_reference_id: Option<&str>,
        receipt: &crate::HostInputReceipt,
    ) -> Result<(), StorageError> {
        self.request(|reply| {
            Command::AppendHostInputAccepted(
                rows.to_vec(),
                edges.to_vec(),
                consumed_reference_id.map(String::from),
                receipt.clone(),
                reply,
            )
        })
    }

    /// Move `Accepted` receipts to a final state without a snapshot; returns
    /// the transitions that took effect.
    ///
    /// # Errors
    /// Returns an error for suspended writes, SQLite failure or a closed writer.
    pub fn transition_host_inputs(
        &self,
        transitions: &[crate::HostInputTransition],
    ) -> Result<Vec<crate::HostInputTransition>, StorageError> {
        self.request(|reply| Command::TransitionHostInputs(transitions.to_vec(), reply))
    }

    /// Mark every `Accepted` receipt of a run that can no longer read it as
    /// `Unknown`; returns the changed prompt IDs.
    ///
    /// # Errors
    /// Returns an error for suspended writes, SQLite failure or a closed writer.
    pub fn mark_host_inputs_unknown(
        &self,
        run_id: &str,
        reason: &str,
    ) -> Result<Vec<String>, StorageError> {
        self.request(|reply| Command::UpdateHostInputReceipts(run_id.into(), reason.into(), reply))
    }

    /// Receipts for the given prompt IDs after all earlier writer requests.
    ///
    /// # Errors
    /// Returns an error for SQLite failure or a closed writer.
    pub fn host_input_receipts(
        &self,
        prompt_ids: &[String],
    ) -> Result<Vec<crate::HostInputReceipt>, StorageError> {
        self.request(|reply| Command::HostInputReceipts(prompt_ids.to_vec(), reply))
    }

    /// One run's receipts, oldest first, after all earlier writer requests.
    ///
    /// # Errors
    /// Returns an error for SQLite failure or a closed writer.
    pub fn host_input_receipts_for_run(
        &self,
        run_id: &str,
    ) -> Result<Vec<crate::HostInputReceipt>, StorageError> {
        self.request(|reply| Command::HostInputReceiptsForRun(run_id.into(), reply))
    }

    /// Change restore bookkeeping only if the exact reserved descriptor still owns it.
    /// History, evidence, reference consumption and timestamps are never rolled back.
    /// Returns false when another snapshot has replaced the reservation.
    ///
    /// # Errors
    /// Returns an error for suspended writes, SQLite failure or a closed writer.
    pub fn compare_and_swap_run_context_restore_state(
        &self,
        run_id: &str,
        expected_config: &str,
        retry_config: &str,
        retry_restorable: bool,
    ) -> Result<bool, StorageError> {
        self.request(|reply| {
            Command::CompareAndSwapRunContextRestoreState(
                run_id.into(),
                expected_config.into(),
                retry_config.into(),
                retry_restorable,
                reply,
            )
        })
    }

    /// Disable restore atomically without reading or rewriting legacy history payloads.
    ///
    /// # Errors
    /// Returns an error for SQLite failure or a closed writer.
    pub fn invalidate_run_context(&self, run_id: &str) -> Result<(), StorageError> {
        self.request(|reply| Command::InvalidateRunContext(run_id.into(), reply))
    }

    pub fn append_fenced_event(
        &self,
        session_id: Option<&str>,
        event: &Event,
        validator: event_bus::MutationValidator,
    ) -> Result<(), StorageError> {
        self.request(|reply| {
            Command::AppendFencedEvent(
                session_id.map(String::from),
                event.clone(),
                validator,
                reply,
            )
        })
    }
    pub fn append_team_snapshot(
        &self,
        snapshot: crate::team::TeamSnapshot,
    ) -> Result<(), StorageError> {
        self.request(|reply| Command::Memory(crate::repo::memory::Mutation::Team(snapshot), reply))
    }
    pub fn append_finding(&self, finding: &crate::memory::Lesson) -> Result<(), StorageError> {
        self.request(|reply| {
            Command::Memory(
                crate::repo::memory::Mutation::Finding(finding.clone()),
                reply,
            )
        })
    }
    pub(crate) fn queue_mutation(
        &self,
        mutation: crate::task_queue::Mutation,
    ) -> Result<(), StorageError> {
        self.request(|reply| Command::TaskQueue(mutation, reply))
    }
    pub fn append_lesson(&self, lesson: &crate::memory::Lesson) -> Result<(), StorageError> {
        self.append_lessons(std::slice::from_ref(lesson))
    }

    pub fn append_lessons(&self, lessons: &[crate::memory::Lesson]) -> Result<(), StorageError> {
        self.request(|reply| {
            Command::Memory(
                crate::repo::memory::Mutation::Candidates(lessons.to_vec()),
                reply,
            )
        })
    }

    /// Record a secret-guarded candidate under explicit deduplication and retention policy.
    /// Policy checks, insertion and eviction run atomically on the single writer.
    pub fn record_improvement_candidate(
        &self,
        project: &str,
        candidate: crate::improvement::NewImprovementCandidate,
        policy: crate::improvement::ImprovementWritePolicy,
    ) -> Result<crate::improvement::ImprovementRecordOutcome, StorageError> {
        self.request(|reply| {
            Command::RecordImprovementCandidate(project.into(), candidate, policy, reply)
        })
    }

    /// Set the review state, returning false when the candidate does not exist.
    pub fn set_improvement_status(
        &self,
        id: &str,
        status: crate::improvement::ImprovementStatus,
    ) -> Result<bool, StorageError> {
        self.request(|reply| Command::SetImprovementStatus(id.into(), status, reply))
    }

    /// Attach a secret-guarded draft reference; return false for an unknown candidate.
    ///
    /// Callers should supply only a filename (basename) or relative path; runtime drafts
    /// default to a managed directory. An absolute host path is the caller's explicit
    /// choice. Storage saves the supplied reference verbatim and performs no file I/O.
    pub fn attach_improvement_draft(
        &self,
        id: &str,
        draft_path: &str,
    ) -> Result<bool, StorageError> {
        self.request(|reply| Command::AttachImprovementDraft(id.into(), draft_path.into(), reply))
    }

    pub fn validate_lesson(&self, id: &str, evidence: &str) -> Result<(), StorageError> {
        self.request(|reply| {
            Command::Memory(
                crate::repo::memory::Mutation::Validate {
                    id: id.into(),
                    evidence: evidence.into(),
                },
                reply,
            )
        })
    }

    pub fn promote_lesson(&self, id: &str) -> Result<(), StorageError> {
        self.request(|reply| {
            Command::Memory(crate::repo::memory::Mutation::Promote(id.into()), reply)
        })
    }

    pub fn reject_lesson(&self, id: &str) -> Result<(), StorageError> {
        self.request(|reply| {
            Command::Memory(crate::repo::memory::Mutation::Reject(id.into()), reply)
        })
    }
    /// イベントを容量制限付きで追記します。
    ///
    /// # Errors
    ///
    /// raw usage event、容量上限超過、writer の終了、credential らしき値の検出
    /// （heuristic、ADR 0008 defense-in-depth）、または永続化処理に失敗した場合に
    /// エラーを返します。
    pub fn append_event(
        &self,
        session_id: Option<&str>,
        event: &Event,
    ) -> Result<(), StorageError> {
        if matches!(event.kind, event_bus::EventKind::Usage(_)) {
            return Err(StorageError::RawUsageEventNotPersisted);
        }
        // 公開 ingress で fail-fast に検査する。writer スレッド側の
        // repo::event::append_event でも再検査される。
        let event = crate::entity::redact_tool_event(event);
        SecretGuard::from_env().check_event_kind(&event.kind)?;
        let (reply, result) = mpsc::channel();
        self.0
            .send(Command::AppendEvent(
                session_id.map(String::from),
                event.clone(),
                reply,
            ))
            .map_err(|_| StorageError::WriterClosed)?;
        result.recv().map_err(|_| StorageError::WriterClosed)?
    }

    /// カタログ更新履歴を writer 経由で保存します。
    ///
    /// # Errors
    ///
    /// writer が終了済み、または SQLite 操作に失敗した場合にエラーを返します。
    pub fn record_catalog_update(&self, record: &CatalogUpdateRecord) -> Result<(), StorageError> {
        let record = record.clone();
        self.request(|reply| Command::RecordCatalogUpdate(record, reply))
    }

    /// イベントログを正として session / task projection を再調整します。
    ///
    /// # Errors
    ///
    /// writer が終了済み、または SQLite 操作に失敗した場合にエラーを返します。
    pub fn reconcile(&self) -> Result<ReconcileSummary, StorageError> {
        let (reply, result) = mpsc::channel();
        self.0
            .send(Command::Reconcile(reply))
            .map_err(|_| StorageError::WriterClosed)?;
        result.recv().map_err(|_| StorageError::WriterClosed)?
    }

    /// usage ledger へリクエスト単位の行を保存します。既存の request ID は維持します。
    ///
    /// # Errors
    ///
    /// 書き込み停止中、writer が終了済み、または SQLite 操作に失敗した場合にエラーを返します。
    pub fn record_usage_requests(
        &self,
        records: Vec<crate::usage::UsageRequestRecord>,
    ) -> Result<(), StorageError> {
        if records.is_empty() {
            return Ok(());
        }
        self.request(|reply| Command::RecordUsageRequests(records, reply))
    }

    /// run の所属 thread / project を待たずに送ります。キューが満杯なら
    /// 送らずに `false` を返すため、呼び出し側は次の機会に再送できます。
    pub fn try_attribute_usage_runs(
        &self,
        attributions: Vec<crate::usage::RunAttribution>,
    ) -> bool {
        if attributions.is_empty() {
            return true;
        }
        match self.0.try_send(Command::AttributeUsageRuns(attributions)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => {
                tracing::warn!("storage writer is closed; dropping usage attribution");
                true
            }
        }
    }

    /// Record a thread the runtime started or branched (ADR 0041). Columns the
    /// host already mirrored are kept; the runtime fills only the ones it left
    /// empty, and `None` never erases a stored value.
    ///
    /// # Errors
    /// Rejects an empty thread ID, suspended writes, SQLite failure or a closed writer.
    pub fn upsert_thread_runtime(
        &self,
        thread: &crate::threads::ThreadUpsert,
    ) -> Result<(), StorageError> {
        self.request(|reply| {
            Command::UpsertThreads(
                crate::threads::ThreadWriter::Runtime,
                vec![thread.clone()],
                Some(reply),
            )
        })
    }

    /// Mirror `sidebar.json` thread definitions and wait for the commit. Every
    /// supplied value replaces the stored one; `None` keeps it.
    ///
    /// # Errors
    /// Rejects the whole batch for an empty thread ID; also fails for suspended
    /// writes, SQLite failure or a closed writer.
    pub fn upsert_threads_host(
        &self,
        threads: &[crate::threads::ThreadUpsert],
    ) -> Result<(), StorageError> {
        if threads.is_empty() {
            return Ok(());
        }
        self.request(|reply| {
            Command::UpsertThreads(
                crate::threads::ThreadWriter::Host,
                threads.to_vec(),
                Some(reply),
            )
        })
    }

    /// Mirror `sidebar.json` thread definitions like [`Self::upsert_threads_host`]
    /// without waiting. Returns `false`, sending nothing, when the queue is full
    /// so the caller can resend on its next tick. Write failures are only logged.
    pub fn try_upsert_threads(&self, threads: Vec<crate::threads::ThreadUpsert>) -> bool {
        if threads.is_empty() {
            return true;
        }
        match self.0.try_send(Command::UpsertThreads(
            crate::threads::ThreadWriter::Host,
            threads,
            None,
        )) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => {
                tracing::warn!("storage writer is closed; dropping thread mirror");
                true
            }
        }
    }

    /// メモリ内の保存統計を返します。SQLiteやイベントへの永続化は行いません。
    pub fn statistics(&self) -> Result<StorageStatistics, StorageError> {
        self.request(Command::Statistics)
    }

    /// 保留中の usage バケットを直ちに永続化します。
    ///
    /// # Errors
    ///
    /// writer が終了済み、または SQLite 操作に失敗した場合にエラーを返します。
    pub fn flush_usage_now(&self) -> Result<(), StorageError> {
        self.request(Command::FlushUsage)
    }

    /// Run one maintenance tick: bounded 30-day diagnostic retention, thread start
    /// retention and incremental vacuum, WAL checkpoint, size reevaluation and temp checks.
    ///
    /// # Errors
    ///
    /// writer が終了済み、SQLite 操作、またはデータベース関連ファイルのサイズ取得に
    /// 失敗した場合にエラーを返します。
    pub fn checkpoint_now(&self) -> Result<(), StorageError> {
        self.request(Command::Checkpoint)
    }

    fn request<T>(&self, command: impl FnOnce(ReplyTx<T>) -> Command) -> Result<T, StorageError> {
        let (reply, result) = mpsc::channel();
        self.0
            .send(command(reply))
            .map_err(|_| StorageError::WriterClosed)?;
        result.recv().map_err(|_| StorageError::WriterClosed)?
    }

    fn index_request<T>(
        &self,
        command: impl FnOnce(index::ReplySender<T, StorageError>) -> index::IndexCommand,
    ) -> Result<T, StorageError> {
        self.request(|reply| Command::Index(command(reply)))
    }

    fn index_control<T>(
        &self,
        command: impl FnOnce(index::ReplySender<T, RequestFailure>) -> index::IndexCommand,
    ) -> Result<T, RequestFailure> {
        let (reply, result) = mpsc::channel();
        self.0
            .send(Command::Index(command(reply)))
            .map_err(|_| RequestFailure::DatabaseUnavailable)?;
        result
            .recv()
            .map_err(|_| RequestFailure::DatabaseUnavailable)?
    }

    /// Makes this process's writer the long-lived (GUI) index owner: it takes
    /// the DB lease when free, renews it, and steps the index in the
    /// background. A CLI must not call this; it submits a request and, when
    /// the submission elected it, steps in the foreground.
    ///
    /// # Errors
    /// Returns an error for SQLite failure or a closed writer.
    pub fn start_thread_index_owner(&self, owner: IndexOwner) -> Result<LeaseResult, StorageError> {
        let sender = self.0.clone();
        self.index_request(|reply| index::IndexCommand::StartOwner(owner, sender, reply))
    }

    /// Stops the background owner and releases its lease.
    ///
    /// # Errors
    /// Returns an error for SQLite failure or a closed writer.
    pub fn stop_thread_index_owner(&self) -> Result<(), StorageError> {
        self.index_request(index::IndexCommand::StopOwner)
    }

    /// Persists a reindex request with its frozen targets and elects the owner
    /// in the same transaction (ADR 0041 R5).
    /// All index-control `now_ns` arguments must be Unix-epoch wall-clock nanoseconds (`SystemTime`), matching admission's resampled clock.
    ///
    /// # Errors
    /// `InvalidArguments` before acceptance, `PersistenceUnavailable` /
    /// `DatabaseUnavailable` when the request could not be stored.
    pub fn submit_thread_index_request(
        &self,
        request: &ReindexRequest,
        owner: &IndexOwner,
        now_ns: i64,
        lease_duration_ns: i64,
    ) -> Result<RequestSubmission, RequestFailure> {
        let request = Box::new(request.clone());
        let owner = owner.clone();
        self.index_control(|reply| {
            index::IndexCommand::Submit(request, owner, now_ns, lease_duration_ns, reply)
        })
    }

    /// Shared request status, including the current lease holder (if any).
    ///
    /// # Errors
    /// `InvalidArguments` for an unknown id.
    pub fn thread_index_request_status(
        &self,
        request_id: &str,
        now_ns: i64,
    ) -> Result<RequestStatus, RequestFailure> {
        let request_id = request_id.to_owned();
        self.index_control(|reply| index::IndexCommand::Status(request_id, now_ns, reply))
    }

    /// Cancels only this request; shared work for other requests continues.
    ///
    /// # Errors
    /// `InvalidArguments` for an unknown id.
    pub fn cancel_thread_index_request(
        &self,
        request_id: &str,
        now_ns: i64,
    ) -> Result<RequestStatus, RequestFailure> {
        let request_id = request_id.to_owned();
        self.index_control(|reply| index::IndexCommand::Cancel(request_id, now_ns, reply))
    }

    /// Fenced lease election (new epoch for a new owner).
    ///
    /// # Errors
    /// Returns an error when the lease row cannot be written.
    pub fn acquire_thread_index_lease(
        &self,
        owner: &IndexOwner,
        now_ns: i64,
        lease_duration_ns: i64,
    ) -> Result<LeaseResult, RequestFailure> {
        let owner = owner.clone();
        self.index_control(|reply| {
            index::IndexCommand::Acquire(owner, now_ns, lease_duration_ns, reply)
        })
    }

    /// Renews only the exact live lease.
    ///
    /// # Errors
    /// Returns an error when the lease row cannot be written.
    pub fn heartbeat_thread_index_lease(
        &self,
        lease: &IndexLease,
        now_ns: i64,
        lease_duration_ns: i64,
    ) -> Result<LeaseResult, RequestFailure> {
        let lease = lease.clone();
        self.index_control(|reply| {
            index::IndexCommand::Heartbeat(lease, now_ns, lease_duration_ns, reply)
        })
    }

    /// Releases the lease if it is still this epoch's.
    ///
    /// # Errors
    /// Returns an error when the lease row cannot be written.
    pub fn release_thread_index_lease(&self, lease: &IndexLease) -> Result<(), RequestFailure> {
        let lease = lease.clone();
        self.index_control(|reply| index::IndexCommand::Release(lease, reply))
    }

    /// One foreground step of the shared index work (CLI owner).
    ///
    /// # Errors
    /// `UnrecoverableControlFailure` when the step could not run.
    pub fn step_thread_index(
        &self,
        lease: &IndexLease,
        now_ns: i64,
    ) -> Result<IndexStepResult, RequestFailure> {
        let lease = lease.clone();
        let sender = self.0.clone();
        self.index_control(|reply| index::IndexCommand::Step(lease, now_ns, sender, reply))
    }

    /// The shared capacity decision (`capacity_status`) with measured sizes.
    ///
    /// # Errors
    /// Returns an error for a file-size or SQLite failure.
    pub fn thread_index_capacity(&self) -> Result<IndexCapacityStatus, StorageError> {
        self.index_request(index::IndexCommand::Capacity)
    }

    /// Applies index settings in writer order; the new limit is effective from
    /// this point. Raising the limit never revives evicted runs.
    ///
    /// # Errors
    /// Returns an error for SQLite failure or a closed writer.
    pub fn apply_thread_index_settings(
        &self,
        settings: ThreadIndexSettings,
    ) -> Result<IndexSettingsReceipt, StorageError> {
        self.index_request(|reply| index::IndexCommand::ApplySettings(settings, reply))
    }

    /// Diagnostics of the background index job (no index content).
    ///
    /// # Errors
    /// Returns an error for a closed writer.
    pub fn thread_index_job_status(&self) -> Result<ThreadIndexJobStatus, StorageError> {
        self.index_request(index::IndexCommand::JobStatus)
    }

    #[cfg(test)]
    fn block_writer(&self, release: mpsc::Receiver<()>) -> Result<(), StorageError> {
        self.0
            .send(Command::Index(index::IndexCommand::Block(release)))
            .map_err(|_| StorageError::WriterClosed)
    }
}

impl UsageSink for StorageHandle {
    fn submit(&self, buckets: Vec<UsageBucket>) {
        if buckets.is_empty() {
            return;
        }
        match self.0.try_send(Command::Usage(buckets)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                tracing::warn!("storage usage queue is full; dropping metrics")
            }
            Err(TrySendError::Disconnected(_)) => {
                tracing::warn!("storage writer is closed; dropping metrics");
            }
        }
    }
}
