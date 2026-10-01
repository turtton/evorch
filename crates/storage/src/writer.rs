//! 専用スレッド上の単一 SQLite writer を管理します。

use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread::JoinHandle;

use event_bus::{Event, UsageBucket, UsageSink};

use crate::db::{file_sizes, temp_files_bytes};
use crate::entity::SecretGuard;
use crate::{CatalogUpdateRecord, Database, ReconcileSummary, StorageConfig, StorageError};

mod state;
#[cfg(test)]
mod stream_tests;

use state::{log_size_state, log_temp_state, run_writer, temp_exceeded};

type ReplyTx<T = ()> = mpsc::Sender<Result<T, StorageError>>;
type ReconcileReplyTx = mpsc::Sender<Result<ReconcileSummary, StorageError>>;

// AgentRunStarted 追加で Event が大型化。Box 化は append hot path に alloc を
// 増やすためサイズを許容する。
#[allow(clippy::large_enum_variant)]
enum Command {
    Usage(Vec<UsageBucket>),
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
    CreateUserQuestion(event_bus::UserQuestion, ReplyTx),
    BindUserQuestions(String, String, Vec<String>, ReplyTx),
    AnswerUserQuestion(String, String, ReplyTx),
    UpsertRunContext(crate::RunContextRecord, ReplyTx),
    InvalidateRunContext(String, ReplyTx),
    Reconcile(ReconcileReplyTx),
    FlushUsage(ReplyTx),
    Checkpoint(ReplyTx),
    Statistics(ReplyTx<StorageStatistics>),
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
        let writer = std::thread::Builder::new()
            .name("storage-writer".into())
            .spawn(move || {
                run_writer(
                    database.conn,
                    rx,
                    config,
                    writes_suspended,
                    soft_warned,
                    writes_suspended,
                    temp_warned,
                )
            })
            .map_err(|error| StorageError::Io(error.to_string()))?;
        Ok(Self(StorageHandle(tx, max_event_bytes), Some(writer)))
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
pub struct StorageHandle(SyncSender<Command>, u64);

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
    pub fn answer_user_question(&self, id: &str, answer: &str) -> Result<(), StorageError> {
        self.request(|reply| Command::AnswerUserQuestion(id.into(), answer.into(), reply))
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

    /// Append a guarded, 1..=8192 byte body and return its global sequence number.
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

    /// Replace the stored snapshot for a run; the last write wins.
    ///
    /// # Errors
    /// Returns an error for suspended writes, SQLite failure or a closed writer.
    pub fn upsert_run_context(&self, record: &crate::RunContextRecord) -> Result<(), StorageError> {
        self.request(|reply| Command::UpsertRunContext(record.clone(), reply))
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
    pub fn append_eval_trace(&self, trace: &crate::eval::EvalTrace) -> Result<(), StorageError> {
        self.request(|reply| {
            Command::Memory(
                crate::repo::memory::Mutation::EvalTrace(trace.clone()),
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

    /// PASSIVE WAL checkpoint、サイズ状態の再評価、閾値条件付きの budgeted incremental
    /// vacuum、および temp 容量検査（maintenance tick）を直ちに実行します。
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
