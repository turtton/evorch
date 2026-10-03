use std::collections::HashMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Instant;

use event_bus::{BucketKey, Event, UsageBucket};
use rusqlite::Connection;

use super::{Command, StorageStatistics};
use crate::db::file_sizes;
use crate::projection;
use crate::repo::{catalog, event, metrics};
use crate::{HardLimits, LimitKind, StorageConfig, StorageError};

#[path = "diagnostic.rs"]
mod diagnostic;

#[cfg(test)]
#[path = "accounting_tests.rs"]
mod accounting_tests;

struct WriterState {
    conn: Connection,
    accounting: event::EventAccounting,
    statistics: StorageStatistics,
    config: StorageConfig,
    pending: HashMap<BucketKey, UsageBucket>,
    writes_suspended: bool,
    soft_warned: bool,
    suspend_logged: bool,
    temp_warned: bool,
    next_flush_at: Instant,
    next_checkpoint_at: Instant,
    diagnostic_cursor: Option<event::diagnostic::Cursor>,
    reclaim_wal_pending: bool,
}

pub(super) fn run_writer(
    conn: Connection,
    rx: Receiver<Command>,
    config: StorageConfig,
    writes_suspended: bool,
    soft_warned: bool,
    suspend_logged: bool,
    temp_warned: bool,
) {
    let now = Instant::now();
    let mut state = WriterState {
        conn,
        accounting: event::EventAccounting::default(),
        statistics: StorageStatistics::default(),
        next_flush_at: now + config.flush_interval,
        next_checkpoint_at: now + config.checkpoint_interval,
        config,
        pending: HashMap::new(),
        writes_suspended,
        soft_warned,
        suspend_logged,
        temp_warned,
        diagnostic_cursor: None,
        reclaim_wal_pending: false,
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
                record_event_failure(&mut state, &result);
                let _ = reply.send(result);
            }
            Ok(Command::AppendFencedEvent(session_id, event, validator, reply)) => {
                let guards = validator.acquire(&event);
                let result = if guards.is_none() {
                    Err(StorageError::StaleMutation)
                } else if state.writes_suspended {
                    handle_suspended_append(&mut state, &session_id, &event)
                } else {
                    append_event_to_conn(&mut state, &session_id, &event)
                };
                record_event_failure(&mut state, &result);
                let _ = reply.send(result);
            }
            Ok(Command::RecordCatalogUpdate(record, reply)) => {
                let _ = reply.send(catalog::record(&state.conn, &record));
            }
            Ok(Command::AppendStreamEvent(stream_id, event, validator, reply)) => {
                let guards = validator
                    .as_ref()
                    .map(|validator| validator.acquire(&event));
                let result = if matches!(guards, Some(None)) {
                    Err(StorageError::StaleMutation)
                } else {
                    let session_limit = state.config.hard_limits.max_session_bytes;
                    state.config.hard_limits.max_session_bytes = u64::MAX;
                    let result = if state.writes_suspended {
                        handle_suspended_append(&mut state, &Some(stream_id), &event)
                    } else {
                        append_event_to_conn(&mut state, &Some(stream_id), &event)
                    };
                    state.config.hard_limits.max_session_bytes = session_limit;
                    result
                };
                record_event_failure(&mut state, &result);
                let _ = reply.send(result);
            }
            Ok(Command::CreateUserQuestion(question, reply)) => {
                let result = if state.writes_suspended {
                    Err(StorageError::Serialization(
                        "question writes suspended by storage limit".into(),
                    ))
                } else {
                    crate::repo::user_questions::create(&state.conn, &question)
                };
                let _ = reply.send(result);
            }
            Ok(Command::BindUserQuestions(source, target, ids, reply)) => {
                let result = if state.writes_suspended {
                    Err(StorageError::Serialization(
                        "question writes suspended by storage limit".into(),
                    ))
                } else {
                    crate::repo::user_questions::bind(&state.conn, &source, &target, &ids)
                };
                let _ = reply.send(result);
            }
            Ok(Command::AnswerUserQuestion(id, answer, reply)) => {
                let result = if state.writes_suspended {
                    Err(StorageError::Serialization(
                        "question writes suspended by storage limit".into(),
                    ))
                } else {
                    crate::repo::user_questions::answer(&state.conn, &id, &answer)
                };
                let _ = reply.send(result);
            }
            Ok(Command::AppendRunLedger(run_id, body, reply)) => {
                let result = if state.writes_suspended {
                    Err(StorageError::Serialization(
                        "run ledger writes suspended by storage limit".into(),
                    ))
                } else {
                    crate::repo::run_ledger::append(&state.conn, &run_id, &body)
                };
                let _ = reply.send(result);
            }
            Ok(Command::UpsertRunContext(record, reply)) => {
                let result = if state.writes_suspended {
                    Err(StorageError::Serialization(
                        "run context writes suspended by storage limit".into(),
                    ))
                } else {
                    crate::repo::run_context::upsert(&state.conn, &record)
                };
                let _ = reply.send(result);
            }
            Ok(Command::InvalidateRunContext(run_id, reply)) => {
                let _ = reply.send(crate::repo::run_context::invalidate(&state.conn, &run_id));
            }
            Ok(Command::Memory(mutation, reply)) => {
                let result = if state.writes_suspended {
                    Err(StorageError::Serialization(
                        "memory writes suspended by storage limit".into(),
                    ))
                } else {
                    crate::repo::memory::append(&state.conn, &mutation)
                };
                let _ = reply.send(result);
            }
            Ok(Command::RecordImprovementCandidate(project, candidate, policy, reply)) => {
                let result = if state.writes_suspended {
                    Err(StorageError::Serialization(
                        "improvement writes suspended by storage limit".into(),
                    ))
                } else {
                    crate::improvement::record(&state.conn, &project, &candidate, policy)
                };
                let _ = reply.send(result);
            }
            Ok(Command::SetImprovementStatus(id, status, reply)) => {
                let result = if state.writes_suspended {
                    Err(StorageError::Serialization(
                        "improvement writes suspended by storage limit".into(),
                    ))
                } else {
                    crate::improvement::set_status(&state.conn, &id, status)
                };
                let _ = reply.send(result);
            }
            Ok(Command::AttachImprovementDraft(id, draft_path, reply)) => {
                let result = if state.writes_suspended {
                    Err(StorageError::Serialization(
                        "improvement writes suspended by storage limit".into(),
                    ))
                } else {
                    crate::improvement::attach_draft(&state.conn, &id, &draft_path)
                };
                let _ = reply.send(result);
            }
            Ok(Command::TaskQueue(mutation, reply)) => {
                let result = if state.writes_suspended {
                    Err(StorageError::Serialization(
                        "queue writes suspended by storage limit".into(),
                    ))
                } else {
                    crate::task_queue::apply(&state.conn, &mutation)
                };
                let _ = reply.send(result);
            }
            Ok(Command::Reconcile(reply)) => {
                let _ = reply.send(projection::reconcile(&state.conn));
            }
            Ok(Command::FlushUsage(reply)) => {
                let result = flush_usage(&mut state);
                let _ = reply.send(result);
            }
            Ok(Command::Checkpoint(reply)) => {
                let result = maintenance(&mut state);
                let _ = reply.send(result);
            }
            Ok(Command::Statistics(reply)) => {
                let _ = reply.send(Ok(state.statistics.clone()));
            }
            Ok(Command::DiagnosticCleanupStatus(scope, reply)) => {
                let _ = reply.send(diagnostic::status(&state, scope));
            }
            Ok(Command::CleanupDiagnosticBatch(scope, cursor, through, reply)) => {
                let _ = reply.send(diagnostic::cleanup_batch(
                    &mut state, scope, cursor, through,
                ));
            }
            Ok(Command::ReclaimDiagnosticSpace(pages, reply)) => {
                let _ = reply.send(diagnostic::reclaim_step(&mut state, pages));
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
        if let Err(error) = maintenance(state) {
            tracing::warn!(error = %error, "storage maintenance failed");
        }
        state.next_checkpoint_at = now + state.config.checkpoint_interval;
    }
}

/// Maintenance scans one bounded diagnostic batch, then reclaims a bounded
/// page budget and checks storage limits against the resulting file sizes.
fn maintenance(state: &mut WriterState) -> Result<(), StorageError> {
    let removed = diagnostic::retain_recent(state, std::time::SystemTime::now())?;
    let mut vacuum_config = state.config.clone();
    if removed > 0 || state.writes_suspended || state.reclaim_wal_pending {
        vacuum_config.vacuum_freelist_threshold_pages = 1;
    }
    run_budgeted_vacuum(&state.conn, &vacuum_config)?;
    if removed > 0 || state.writes_suspended || state.reclaim_wal_pending {
        diagnostic::reclaim_wal(state)?;
    }
    checkpoint(state)?;
    check_temp(state)?;
    state.statistics.maintenance_runs = state.statistics.maintenance_runs.saturating_add(1);
    Ok(())
}

/// freelist が設定閾値以上のときだけ、設定 page budget 以内で incremental vacuum を実行し、
/// 回収前後の page 数を診断ログへ出力します。budget 0 は回収を無効化します。
fn run_budgeted_vacuum(conn: &Connection, config: &StorageConfig) -> Result<(), StorageError> {
    let threshold = config.vacuum_freelist_threshold_pages;
    let budget = config.vacuum_page_budget_per_tick;
    if budget == 0 {
        return Ok(());
    }
    let before = freelist_pages(conn)?;
    if before == 0 || before < threshold {
        return Ok(());
    }
    // rusqlite の ToSql は u64 未対応のため i64 へ飽和変換します。
    let pages = i64::try_from(budget.min(before)).unwrap_or(i64::MAX);
    diagnostic::vacuum_pages(conn, pages)?;
    let after = freelist_pages(conn)?;
    tracing::info!(
        freelist_before_pages = before,
        freelist_after_pages = after,
        pages_reclaimed = before.saturating_sub(after),
        page_budget = budget,
        "incremental vacuum completed"
    );
    Ok(())
}

fn freelist_pages(conn: &Connection) -> Result<u64, StorageError> {
    let count: i64 = conn.pragma_query_value(None, "freelist_count", |row| row.get(0))?;
    u64::try_from(count).map_err(|_| StorageError::OutOfRange("freelist_count"))
}

/// temp 副産物が `bytes >= threshold` で実際に存在するとき超過とみなします。
/// `bytes == 0` のとき閾値 0 で即警告とならないよう、存在を条件に含めます。
pub(super) fn temp_exceeded(bytes: u64, threshold: u64) -> bool {
    bytes > 0 && bytes >= threshold
}

/// temp 副産物の合計バイト数を測定し、閾値の超過/復帰遷移時のみ診断イベントを出力します。
fn check_temp(state: &mut WriterState) -> Result<(), StorageError> {
    let bytes = crate::db::temp_files_bytes(&state.config.db_path)?;
    let threshold = state.config.temp_warn_bytes;
    if temp_exceeded(bytes, threshold) {
        if !state.temp_warned {
            tracing::warn!(
                temp_bytes = bytes,
                threshold_bytes = threshold,
                "temp storage threshold exceeded"
            );
            state.temp_warned = true;
        }
    } else if state.temp_warned {
        tracing::info!(
            temp_bytes = bytes,
            threshold_bytes = threshold,
            "temp storage within threshold"
        );
        state.temp_warned = false;
    }
    Ok(())
}

/// 起動時の temp 容量検査結果を出力します。超過時のみ 1 回警告します。
pub(super) fn log_temp_state(temp_bytes: u64, threshold_bytes: u64, warned: bool) {
    if warned {
        tracing::warn!(
            temp_bytes,
            threshold_bytes,
            "temp storage threshold exceeded"
        );
    }
}

fn flush_usage(state: &mut WriterState) -> Result<(), StorageError> {
    if state.pending.is_empty() {
        return Ok(());
    }
    let buckets = state.pending.values().cloned().collect::<Vec<_>>();
    metrics::upsert_buckets(&state.conn, &buckets)?;
    state.pending.clear();
    state.statistics.usage_flushes = state.statistics.usage_flushes.saturating_add(1);
    state.statistics.usage_buckets_written = state
        .statistics
        .usage_buckets_written
        .saturating_add(buckets.len() as u64);
    tracing::debug!(bucket_count = buckets.len(), "usage flush");
    Ok(())
}

fn append_event_to_conn(
    state: &mut WriterState,
    session_id: &Option<String>,
    event: &Event,
) -> Result<(), StorageError> {
    // 同じセッション・日付の集計を保持し、ストリーム差分ごとの履歴再走査を避けます。
    let started = Instant::now();
    let result = event::append_event(
        &state.conn,
        session_id.as_deref(),
        event,
        &state.config.hard_limits,
        &mut state.accounting,
    )
    .map(|_| ());
    let elapsed = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    state.statistics.append_total_micros =
        state.statistics.append_total_micros.saturating_add(elapsed);
    state.statistics.max_append_micros = state.statistics.max_append_micros.max(elapsed);
    if result.is_ok() {
        state.statistics.events_written = state.statistics.events_written.saturating_add(1);
        state.statistics.event_payload_bytes = state
            .statistics
            .event_payload_bytes
            .saturating_add(state.accounting.last_event_bytes);
    }
    result
}

fn record_event_failure(state: &mut WriterState, result: &Result<(), StorageError>) {
    if result.is_err() {
        state.statistics.event_write_failures =
            state.statistics.event_write_failures.saturating_add(1);
    }
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
        diagnostic::reclaim_wal(state)?;
        sizes = file_sizes(&state.config.db_path)?;
    }
    refresh_size_state(state, sizes.total());
    Ok(())
}

fn refresh_size_state(state: &mut WriterState, total_bytes: u64) {
    let suspended = total_bytes >= state.config.hard_limits.max_db_bytes;
    let threshold =
        state.config.hard_limits.max_db_bytes as f64 * state.config.hard_limits.soft_warn_ratio;
    if suspended {
        if !state.suspend_logged {
            tracing::error!(
                total_bytes,
                max_bytes = state.config.hard_limits.max_db_bytes,
                "event writes suspended"
            );
            state.suspend_logged = true;
        }
        state.soft_warned = false;
    } else {
        state.suspend_logged = false;
        if total_bytes as f64 >= threshold {
            if !state.soft_warned {
                tracing::warn!(
                    total_bytes,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<String>>);

    impl std::io::Write for LogBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("log buffer lock")
                .push_str(&String::from_utf8_lossy(buf));
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for LogBuffer {
        type Writer = LogBuffer;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture_logs(action: impl FnOnce()) -> String {
        let buffer = LogBuffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, action);
        buffer.0.lock().expect("log buffer lock").clone()
    }

    fn freelist_pages(conn: &Connection) -> u64 {
        let count: i64 = conn
            .pragma_query_value(None, "freelist_count", |row| row.get(0))
            .expect("freelist_count must read");
        u64::try_from(count).expect("freelist_count is never negative")
    }

    fn grow_freelist(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE filler (x BLOB);
             INSERT INTO filler VALUES (zeroblob(32768)),
              (zeroblob(32768)), (zeroblob(32768)), (zeroblob(32768)),
              (zeroblob(32768)), (zeroblob(32768)), (zeroblob(32768)), (zeroblob(32768));
             DROP TABLE filler;",
        )
        .expect("freelist fixture must run");
    }

    #[test]
    fn budgeted_vacuum_reclaims_freelist_within_budget_per_tick() {
        // Given: freelist を持つ in-memory incremental DB と page budget 2 の設定
        let conn = Connection::open_in_memory().expect("in-memory DB must open");
        conn.pragma_update(None, "auto_vacuum", 2)
            .expect("auto_vacuum must enable");
        grow_freelist(&conn);
        let before = freelist_pages(&conn);
        assert!(before > 4, "freelist fixture too small: {before}");
        let config = StorageConfig {
            vacuum_freelist_threshold_pages: 1,
            vacuum_page_budget_per_tick: 2,
            ..Default::default()
        };

        // When: 1 tick 分の budgeted vacuum を実行する
        run_budgeted_vacuum(&conn, &config).expect("vacuum must run");

        // Then: budget 以内で段階的に回収される
        let after = freelist_pages(&conn);
        assert!(after < before, "freelist must shrink: {before} -> {after}");
        assert!(before - after <= 2, "reclaim must respect budget");
    }

    #[test]
    fn budgeted_vacuum_is_idle_below_freelist_threshold() {
        // Given: threshold 未満の freelist を持つ DB
        let conn = Connection::open_in_memory().expect("in-memory DB must open");
        conn.pragma_update(None, "auto_vacuum", 2)
            .expect("auto_vacuum must enable");
        grow_freelist(&conn);
        let before = freelist_pages(&conn);
        let config = StorageConfig {
            vacuum_freelist_threshold_pages: before + 1,
            ..Default::default()
        };

        // When: 2 tick 連続でメンテナンスを試みる
        run_budgeted_vacuum(&conn, &config).expect("vacuum must be a no-op");
        run_budgeted_vacuum(&conn, &config).expect("vacuum must be a no-op");

        // Then: freelist は一切回収されない
        assert_eq!(freelist_pages(&conn), before);
    }

    #[test]
    fn budgeted_vacuum_with_zero_budget_is_disabled() {
        // Given: budget 0(無効化)の設定と freelist
        let conn = Connection::open_in_memory().expect("in-memory DB must open");
        conn.pragma_update(None, "auto_vacuum", 2)
            .expect("auto_vacuum must enable");
        grow_freelist(&conn);
        let before = freelist_pages(&conn);
        let config = StorageConfig {
            vacuum_freelist_threshold_pages: 1,
            vacuum_page_budget_per_tick: 0,
            ..Default::default()
        };

        // When: メンテナンスを試みる
        run_budgeted_vacuum(&conn, &config).expect("disabled vacuum must be a no-op");

        // Then: freelist は変化しない
        assert_eq!(freelist_pages(&conn), before);
    }

    fn temp_state(db_path: std::path::PathBuf, temp_warn_bytes: u64) -> WriterState {
        let conn = Connection::open_in_memory().expect("in-memory DB must open");
        let config = StorageConfig {
            db_path,
            temp_warn_bytes,
            ..Default::default()
        };
        let now = Instant::now();
        WriterState {
            conn,
            accounting: event::EventAccounting::default(),
            statistics: StorageStatistics::default(),
            temp_warned: false,
            next_flush_at: now,
            next_checkpoint_at: now,
            config,
            pending: HashMap::new(),
            writes_suspended: false,
            soft_warned: false,
            suspend_logged: false,
            diagnostic_cursor: None,
            reclaim_wal_pending: false,
        }
    }

    #[test]
    fn temp_check_warns_once_per_threshold_transition() {
        // Given: threshold(10)超過の journal 副産物を持つ DB パス
        let temp_dir = tempfile::tempdir().expect("temporary directory must be created");
        let db_path = temp_dir.path().join("t.db");
        std::fs::File::create(db::suffixed_path(&db_path, "-journal"))
            .expect("journal must be created")
            .set_len(64)
            .expect("journal size must be set");
        let mut state = temp_state(db_path.clone(), 10);

        // When: 超過状態のまま 2 tick 評価する
        let logs = capture_logs(|| {
            check_temp(&mut state).expect("temp check");
            check_temp(&mut state).expect("temp check");
        });
        assert_eq!(
            logs.matches("temp storage threshold exceeded").count(),
            1,
            "warning must fire once while exceeded: {logs}"
        );

        // When: 閾値未満へ復帰する
        std::fs::remove_file(db::suffixed_path(&db_path, "-journal"))
            .expect("journal must be removed");
        let logs = capture_logs(|| {
            check_temp(&mut state).expect("temp check");
            check_temp(&mut state).expect("temp check");
        });
        // Then: 復帰 1 回だけ通知され、警告は再発しない
        assert!(!logs.contains("temp storage threshold exceeded"));
        assert_eq!(logs.matches("temp storage within threshold").count(), 1);

        // When: 再超過する
        std::fs::File::create(db::suffixed_path(&db_path, "-journal"))
            .expect("journal must be created")
            .set_len(64)
            .expect("journal size must be set");
        let logs = capture_logs(|| {
            check_temp(&mut state).expect("temp check");
        });
        // Then: 新しい遷移として 1 回だけ警告される
        assert_eq!(logs.matches("temp storage threshold exceeded").count(), 1);
    }

    #[test]
    fn temp_check_is_silent_below_threshold() {
        // Given: threshold 未満の journal 副産物
        let temp_dir = tempfile::tempdir().expect("temporary directory must be created");
        let db_path = temp_dir.path().join("t.db");
        std::fs::File::create(db::suffixed_path(&db_path, "-journal"))
            .expect("journal must be created")
            .set_len(5)
            .expect("journal size must be set");
        let mut state = temp_state(db_path, 10_000);

        // When: 2 tick 評価する
        let logs = capture_logs(|| {
            check_temp(&mut state).expect("temp check");
            check_temp(&mut state).expect("temp check");
        });

        // Then: 警告も復帰通知も出ない
        assert!(!logs.contains("temp storage threshold exceeded"));
        assert!(!logs.contains("temp storage within threshold"));
    }
}
