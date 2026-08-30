//! 専用スレッド上の単一 SQLite writer を管理します。

use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread::JoinHandle;

use event_bus::{Event, UsageBucket, UsageSink};

use crate::db::file_sizes;
use crate::{CatalogUpdateRecord, Database, ReconcileSummary, StorageConfig, StorageError};

mod state;

use state::{log_size_state, run_writer};

type ReplyTx = mpsc::Sender<Result<(), StorageError>>;
type ReconcileReplyTx = mpsc::Sender<Result<ReconcileSummary, StorageError>>;

enum Command {
    Usage(Vec<UsageBucket>),
    AppendEvent(Option<String>, Event, ReplyTx),
    RecordCatalogUpdate(CatalogUpdateRecord, ReplyTx),
    Reconcile(ReconcileReplyTx),
    FlushUsage(ReplyTx),
    Checkpoint(ReplyTx),
    Shutdown,
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
        let (tx, rx) = mpsc::sync_channel(config.channel_capacity);
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
                )
            })
            .map_err(|error| StorageError::Io(error.to_string()))?;
        Ok(Self(StorageHandle(tx), Some(writer)))
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
#[derive(Clone)]
pub struct StorageHandle(SyncSender<Command>);

impl StorageHandle {
    /// イベントを容量制限付きで追記します。
    pub fn append_event(
        &self,
        session_id: Option<&str>,
        event: &Event,
    ) -> Result<(), StorageError> {
        if matches!(event.kind, event_bus::EventKind::Usage(_)) {
            return Err(StorageError::RawUsageEventNotPersisted);
        }
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

    /// 保留中の usage バケットを直ちに永続化します。
    pub fn flush_usage_now(&self) -> Result<(), StorageError> {
        self.request(Command::FlushUsage)
    }

    /// PASSIVE WAL checkpoint とサイズ状態の再評価を直ちに実行します。
    pub fn checkpoint_now(&self) -> Result<(), StorageError> {
        self.request(Command::Checkpoint)
    }

    fn request(&self, command: impl FnOnce(ReplyTx) -> Command) -> Result<(), StorageError> {
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
