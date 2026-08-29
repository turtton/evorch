//! 専用スレッド上の単一 SQLite writer を管理します。

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::Instant;

use event_bus::{BucketKey, Event, UsageBucket, UsageSink};
use rusqlite::Connection;

use crate::db::file_sizes;
use crate::repo::{event, metrics};
use crate::{Database, HardLimits, LimitKind, StorageConfig, StorageError};

type ReplyTx = mpsc::Sender<Result<(), StorageError>>;

enum Command {
    Usage(Vec<UsageBucket>),
    AppendEvent(Option<String>, Event, ReplyTx),
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
        let (tx, rx) = mpsc::sync_channel(config.channel_capacity);
        let writer = std::thread::Builder::new()
            .name("storage-writer".into())
            .spawn(move || run_writer(database.conn, rx, config, writes_suspended))
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

struct WriterState {
    conn: Connection,
    config: StorageConfig,
    pending: HashMap<BucketKey, UsageBucket>,
    writes_suspended: bool,
    next_flush_at: Instant,
    next_checkpoint_at: Instant,
}

fn run_writer(
    conn: Connection,
    rx: Receiver<Command>,
    config: StorageConfig,
    writes_suspended: bool,
) {
    let now = Instant::now();
    let mut state = WriterState {
        conn,
        next_flush_at: now + config.flush_interval,
        next_checkpoint_at: now + config.checkpoint_interval,
        config,
        pending: HashMap::new(),
        writes_suspended,
    };
    loop {
        let deadline = state.next_flush_at.min(state.next_checkpoint_at);
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Command::Usage(buckets)) => merge_usage(&mut state.pending, buckets),
            Ok(Command::AppendEvent(session_id, event, reply)) => {
                let result = if state.writes_suspended {
                    file_sizes(&state.config.db_path).and_then(|sizes| {
                        Err(StorageError::LimitExceeded {
                            limit: LimitKind::DbSize,
                            actual: sizes.total(),
                            max: state.config.hard_limits.max_db_bytes,
                        })
                    })
                } else {
                    // セッション切替と日次集計を常に DB から再シードし、キャッシュ不整合を避けます。
                    let mut accounting = event::EventAccounting::default();
                    event::append_event(
                        &state.conn,
                        session_id.as_deref(),
                        &event,
                        &state.config.hard_limits,
                        &mut accounting,
                    )
                    .map(|_| ())
                };
                let _ = reply.send(result);
            }
            Ok(Command::FlushUsage(reply)) => {
                let result = flush_usage(&mut state);
                let _ = reply.send(result);
            }
            Ok(Command::Checkpoint(reply)) => {
                let result = checkpoint(&mut state);
                let _ = reply.send(result);
            }
            Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                if let Err(error) = flush_usage(&mut state) {
                    tracing::warn!(error = %error, "storage final usage flush failed");
                }
                if let Err(error) = state.conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)") {
                    tracing::warn!(error = %error, "storage final checkpoint failed");
                }
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        run_due_work(&mut state);
    }
}

fn merge_usage(pending: &mut HashMap<BucketKey, UsageBucket>, buckets: Vec<UsageBucket>) {
    for bucket in buckets {
        pending
            .entry(bucket.key.clone())
            .and_modify(|current| {
                macro_rules! add {
                    ($field:ident) => {
                        current.$field = current.$field.saturating_add(bucket.$field)
                    };
                }
                add!(input_tokens);
                add!(output_tokens);
                add!(cache_read_tokens);
                add!(cache_write_tokens);
                add!(cache_hits);
                add!(cache_misses);
                add!(request_count);
            })
            .or_insert(bucket);
    }
}

fn run_due_work(state: &mut WriterState) {
    let now = Instant::now();
    if (state.pending.len() >= state.config.flush_max_pending
        || (now >= state.next_flush_at && !state.pending.is_empty()))
        && let Err(error) = flush_usage(state)
    {
        tracing::warn!(error = %error, "storage usage flush failed");
    }
    if now >= state.next_flush_at {
        state.next_flush_at = now + state.config.flush_interval;
    }
    if now >= state.next_checkpoint_at {
        if let Err(error) = checkpoint(state) {
            tracing::warn!(error = %error, "storage checkpoint failed");
        }
        state.next_checkpoint_at = now + state.config.checkpoint_interval;
    }
}

fn flush_usage(state: &mut WriterState) -> Result<(), StorageError> {
    let buckets = state.pending.values().cloned().collect::<Vec<_>>();
    metrics::upsert_buckets(&state.conn, &buckets)?;
    state.pending.clear();
    tracing::debug!(bucket_count = buckets.len(), "usage flush");
    Ok(())
}

fn checkpoint(state: &mut WriterState) -> Result<(), StorageError> {
    state.conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)")?;
    let mut sizes = file_sizes(&state.config.db_path)?;
    if sizes.wal > state.config.hard_limits.max_wal_bytes {
        tracing::warn!(
            wal_bytes = sizes.wal,
            max_bytes = state.config.hard_limits.max_wal_bytes,
            "WAL truncate"
        );
        state
            .conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        sizes = file_sizes(&state.config.db_path)?;
    }
    let suspended = sizes.total() >= state.config.hard_limits.max_db_bytes;
    if suspended != state.writes_suspended {
        log_size_state(sizes.total(), &state.config.hard_limits, suspended);
        state.writes_suspended = suspended;
    } else {
        log_size_state(sizes.total(), &state.config.hard_limits, false);
    }
    Ok(())
}

fn log_size_state(total: u64, limits: &HardLimits, suspended: bool) {
    if suspended {
        tracing::error!(
            total_bytes = total,
            max_bytes = limits.max_db_bytes,
            "event writes suspended"
        );
    } else if total as f64 >= limits.max_db_bytes as f64 * limits.soft_warn_ratio {
        tracing::warn!(
            total_bytes = total,
            max_bytes = limits.max_db_bytes,
            "storage soft limit"
        );
    }
}
