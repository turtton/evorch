use std::collections::HashMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Instant;

use event_bus::{BucketKey, Event, UsageBucket};
use rusqlite::Connection;

use super::Command;
use crate::db::file_sizes;
use crate::projection;
use crate::repo::{catalog, event, metrics};
use crate::{HardLimits, LimitKind, StorageConfig, StorageError};

struct WriterState {
    conn: Connection,
    config: StorageConfig,
    pending: HashMap<BucketKey, UsageBucket>,
    writes_suspended: bool,
    soft_warned: bool,
    suspend_logged: bool,
    next_flush_at: Instant,
    next_checkpoint_at: Instant,
}

pub(super) fn run_writer(
    conn: Connection,
    rx: Receiver<Command>,
    config: StorageConfig,
    writes_suspended: bool,
    soft_warned: bool,
    suspend_logged: bool,
) {
    let now = Instant::now();
    let mut state = WriterState {
        conn,
        next_flush_at: now + config.flush_interval,
        next_checkpoint_at: now + config.checkpoint_interval,
        config,
        pending: HashMap::new(),
        writes_suspended,
        soft_warned,
        suspend_logged,
    };
    loop {
        let deadline = state.next_flush_at.min(state.next_checkpoint_at);
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Command::Usage(buckets)) => merge_usage(&mut state.pending, buckets),
            Ok(Command::AppendEvent(session_id, event, reply)) => {
                let result = if state.writes_suspended {
                    handle_suspended_append(&mut state, &session_id, &event)
                } else {
                    append_event_to_conn(&mut state, &session_id, &event)
                };
                let _ = reply.send(result);
            }
            Ok(Command::RecordCatalogUpdate(record, reply)) => {
                let _ = reply.send(catalog::record(&state.conn, &record));
            }
            Ok(Command::Reconcile(reply)) => {
                let _ = reply.send(projection::reconcile(&state.conn));
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

fn append_event_to_conn(
    state: &mut WriterState,
    session_id: &Option<String>,
    event: &Event,
) -> Result<(), StorageError> {
    // セッション切替と日次集計を常に DB から再シードし、キャッシュ不整合を避けます。
    let mut accounting = event::EventAccounting::default();
    event::append_event(
        &state.conn,
        session_id.as_deref(),
        event,
        &state.config.hard_limits,
        &mut accounting,
    )
    .map(|_| ())
}

fn handle_suspended_append(
    state: &mut WriterState,
    session_id: &Option<String>,
    event: &Event,
) -> Result<(), StorageError> {
    let sizes = file_sizes(&state.config.db_path)?;
    if sizes.total() < state.config.hard_limits.max_db_bytes {
        state.writes_suspended = false;
        state.soft_warned = false;
        state.suspend_logged = false;
        tracing::info!(
            total_bytes = sizes.total(),
            max_bytes = state.config.hard_limits.max_db_bytes,
            "event writes resumed"
        );
        append_event_to_conn(state, session_id, event)
    } else {
        Err(StorageError::LimitExceeded {
            limit: LimitKind::DbSize,
            actual: sizes.total(),
            max: state.config.hard_limits.max_db_bytes,
        })
    }
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
    let threshold =
        state.config.hard_limits.max_db_bytes as f64 * state.config.hard_limits.soft_warn_ratio;
    if suspended {
        if !state.suspend_logged {
            tracing::error!(
                total_bytes = sizes.total(),
                max_bytes = state.config.hard_limits.max_db_bytes,
                "event writes suspended"
            );
            state.suspend_logged = true;
        }
        state.soft_warned = false;
    } else {
        state.suspend_logged = false;
        if sizes.total() as f64 >= threshold {
            if !state.soft_warned {
                tracing::warn!(
                    total_bytes = sizes.total(),
                    max_bytes = state.config.hard_limits.max_db_bytes,
                    "storage soft limit"
                );
                state.soft_warned = true;
            }
        } else {
            state.soft_warned = false;
        }
    }
    state.writes_suspended = suspended;
    Ok(())
}

pub(super) fn log_size_state(total: u64, limits: &HardLimits, suspended: bool) {
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
