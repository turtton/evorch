//! History-only lifecycle. Readers prepare work; the normal writer commits it.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::mpsc,
    thread::JoinHandle,
    time::Duration,
};

use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use super::{
    build::{self, BuildBudget, BuildProgress, Clock, SystemClock},
    index,
};
use crate::{Database, StorageConfig, StorageError, StorageHandle};

type Reply<T> = mpsc::Sender<Result<T, StorageError>>;
pub(crate) enum Write {
    Start(String, BuildBudget, Reply<BuildProgress>),
    Apply(Box<build::PreparedStep>, Reply<BuildProgress>),
    Discover(i64, Vec<(i64, Option<String>)>, Reply<usize>),
}

pub(crate) fn write(conn: &Connection, suspended: bool, command: Write) {
    fn gate(suspended: bool) -> Result<(), StorageError> {
        if suspended {
            Err(StorageError::Io(
                "history construction suspended by storage limit".into(),
            ))
        } else {
            Ok(())
        }
    }
    match command {
        Write::Start(run, budget, reply) => {
            let _ = reply.send(gate(suspended).and_then(|()| build::start(conn, &run, budget)));
        }
        Write::Apply(batch, reply) => {
            let _ = reply.send(gate(suspended).and_then(|()| build::apply(conn, *batch)));
        }
        Write::Discover(after, rows, reply) => {
            let _ = reply.send(gate(suspended).and_then(|()| discover(conn, after, rows)));
        }
    }
}

fn discover(
    conn: &Connection,
    after: i64,
    rows: Vec<(i64, Option<String>)>,
) -> Result<usize, StorageError> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    if !index::version_ok(&tx)? {
        return Ok(0);
    }
    let next = rows.last().map_or(after, |r| r.0);
    if tx.execute(
        "UPDATE history_control SET discovery_after=?2 WHERE id=1 AND discovery_after=?1",
        params![after, next],
    )? == 0
    {
        return Ok(0);
    }
    let before = tx.total_changes();
    let runs: BTreeSet<_> = rows.into_iter().filter_map(|r| r.1).collect();
    for run in runs {
        tx.execute("INSERT OR IGNORE INTO history_run_state(run_id) SELECT ?1 WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=?1)", [run])?;
    }
    let writes = (tx.total_changes() - before) as usize + 1;
    tx.commit()?;
    Ok(writes)
}

#[derive(Clone, Copy)]
struct Retry {
    generation: Option<i64>,
    source: Option<i64>,
    due: i64,
    failures: u32,
    logged_at: i64,
}

/// Scheduling hints only. Every decision rechecks durable state in a snapshot or
/// writer CAS; losing this object cannot publish partially constructed history.
#[derive(Default)]
pub struct Driver {
    after_run: String,
    active: Option<String>,
    retries: BTreeMap<String, Retry>,
    next_discovery: i64,
}

#[derive(Debug, Default)]
pub struct Tick {
    pub run: Option<String>,
    pub progress: BuildProgress,
    pub discovery_rows: usize,
    pub discovery_bytes: usize,
    pub discovery_writes: usize,
}

impl Driver {
    /// One bounded discovery plus one bounded source/cleanup step. The clock is
    /// injectable so debounce and exponential retry tests never wait in real time.
    pub fn tick(
        &mut self,
        reader: &Database,
        writer: &StorageHandle,
        budget: BuildBudget,
        clock: &impl Clock,
    ) -> Result<Tick, StorageError> {
        if budget.rows == 0 || budget.bytes == 0 || budget.writes == 0 {
            return Ok(Tick::default());
        }
        let mut tick = Tick::default();
        let mut budget = budget;
        let now = clock.now_ns();
        if !index::version_ok(&reader.conn)? {
            return Ok(Tick::default());
        }
        if now >= self.next_discovery {
            let after: i64 = reader.conn.query_row(
                "SELECT discovery_after FROM history_control WHERE id=1",
                [],
                |r| r.get(0),
            )?;
            let mut statement = reader.conn.prepare_cached("SELECT id,CASE WHEN octet_length(run_id)<=1024 THEN run_id END FROM events WHERE id>?1 ORDER BY id LIMIT ?2")?;
            let rows: Vec<(i64, Option<String>)> = statement
                .query_map(
                    params![
                        after,
                        budget
                            .rows
                            .min(budget.writes.saturating_sub(1))
                            .min(budget.bytes / 1024)
                            .min(128) as u64
                    ],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?
                .collect::<Result<_, _>>()?;
            if rows.is_empty() {
                self.next_discovery = now.saturating_add(1_000_000_000);
            } else {
                tick.discovery_rows = rows.len();
                tick.discovery_bytes = rows
                    .iter()
                    .map(|r| r.1.as_ref().map_or(0, String::len))
                    .sum();
                tick.discovery_writes = writer.history_discover(after, rows)?;
                budget.rows = budget.rows.saturating_sub(tick.discovery_rows);
                budget.bytes = budget.bytes.saturating_sub(tick.discovery_bytes);
                budget.writes = budget.writes.saturating_sub(tick.discovery_writes);
            }
        }
        if budget.rows == 0 || budget.bytes == 0 || budget.writes == 0 {
            return Ok(tick);
        }
        if let Some(run) = &self.active {
            let current = index::state(&reader.conn, run)?;
            if current
                .as_ref()
                .is_none_or(|s| s.phase == 0 || s.generation.is_none() || s.source.is_none())
            {
                self.active = None;
            }
        }
        if self.active.is_none() {
            let mut statement = reader.conn.prepare_cached("SELECT run_id,phase,source,applied,generation FROM history_run_state WHERE run_id>?1 ORDER BY run_id LIMIT 64")?;
            let mut rows = statement
                .query_map([&self.after_run], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            if rows.is_empty() {
                self.after_run.clear();
                rows = statement
                    .query_map([&self.after_run], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, Option<i64>>(2)?,
                            r.get::<_, Option<i64>>(3)?,
                            r.get::<_, Option<i64>>(4)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
            }
            for (run, phase, source, applied, generation) in rows {
                self.after_run = run.clone();
                if source.is_none() || generation.is_none() || (phase == 2 && source == applied) {
                    self.retries.remove(&run);
                    continue;
                }
                let retry = self.retries.entry(run.clone()).or_insert(Retry {
                    generation,
                    source,
                    due: if phase == 0 {
                        now.saturating_add(250_000_000)
                    } else {
                        now
                    },
                    failures: 0,
                    logged_at: now,
                });
                if phase == 0 && (retry.source != source || retry.generation != generation) {
                    *retry = Retry {
                        source,
                        generation,
                        due: now.saturating_add(250_000_000),
                        failures: 0,
                        logged_at: now,
                    };
                }
                if now >= retry.due {
                    self.active = Some(run);
                    break;
                }
            }
        }
        let Some(run) = self.active.clone() else {
            return Ok(tick);
        };
        let result = (|| {
            let start = writer.history_start(&run, budget)?;
            if start.ready || start.writes >= budget.writes {
                return Ok(start);
            }
            match build::prepare(reader, &run, budget, clock, start)? {
                Some(batch) => writer.history_apply(batch),
                None => Ok(start),
            }
        })();
        let retry = self
            .retries
            .get_mut(&run)
            .expect("active run has retry state");
        match result {
            Ok(progress) => {
                if progress.ready || now.saturating_sub(retry.logged_at) >= 5_000_000_000 {
                    tracing::info!(
                        run,
                        rows = progress.rows,
                        bytes = progress.bytes,
                        writes = progress.writes,
                        ready = progress.ready,
                        "history construction progress"
                    );
                    retry.logged_at = now;
                }
                if progress.ready
                    || progress.conflicted
                    || (progress.rows == 0 && progress.writes == 0)
                {
                    self.active = None;
                }
                tick.run = Some(run);
                tick.progress = progress;
                Ok(tick)
            }
            Err(error) => {
                let reason = match &error {
                    StorageError::Sqlite(_) => "sqlite",
                    StorageError::Serialization(_) => "state_decode",
                    _ => "source_or_budget",
                };
                retry.failures = retry.failures.saturating_add(1);
                retry.due = now
                    .saturating_add(1_000_000_000i64.saturating_mul(1i64 << retry.failures.min(6)));
                // Never log payloads, SQLite values or decode errors.
                tracing::warn!(
                    run,
                    reason,
                    failures = retry.failures,
                    retry_at_ns = retry.due,
                    "history construction retry"
                );
                self.active = None;
                Err(error)
            }
        }
    }
}

/// Owned by the core storage lifecycle, not by the FTS owner. Drop stops new ticks
/// and joins after the current bounded checkpoint has committed.
pub struct Owner {
    stop: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
}
impl Owner {
    pub fn start(config: StorageConfig, writer: StorageHandle) -> Result<Self, StorageError> {
        let (stop, receiver) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("history-builder".into())
            .spawn(move || {
                let mut driver = Driver::default();
                let mut reader = None;
                let mut delay = Duration::from_millis(1);
                loop {
                    match receiver.recv_timeout(delay) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    if reader.is_none() {
                        match Database::open_read_only(&config.db_path) {
                            Ok(db) => reader = Some(db),
                            Err(_) => {
                                delay = Duration::from_secs(5);
                                continue;
                            }
                        }
                    }
                    let clock = SystemClock;
                    let start = clock.now_ns();
                    let mut budget = BuildBudget::default();
                    delay = Duration::from_millis(1);
                    loop {
                        if receiver.try_recv().is_ok() {
                            return;
                        }
                        match driver.tick(reader.as_ref().unwrap(), &writer, budget, &clock) {
                            Ok(tick) => {
                                budget.rows = budget
                                    .rows
                                    .saturating_sub(tick.progress.rows + tick.discovery_rows);
                                budget.bytes = budget
                                    .bytes
                                    .saturating_sub(tick.progress.bytes + tick.discovery_bytes);
                                budget.writes = budget
                                    .writes
                                    .saturating_sub(tick.progress.writes + tick.discovery_writes);
                                if tick.run.is_none() && tick.discovery_rows == 0 {
                                    delay = Duration::from_millis(50);
                                    break;
                                }
                            }
                            Err(_) => {
                                delay = Duration::from_millis(250);
                                break;
                            }
                        }
                        // Reserve the hard maximum single-message work before
                        // another source step, even when the previous one was tiny.
                        if budget.rows < 4096
                            || budget.bytes < 16 * 1024 * 1024
                            || budget.writes < 8196
                            || clock.now_ns().saturating_sub(start) >= 10_000_000
                        {
                            break;
                        }
                    }
                }
            })
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
    pub fn stop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.stop.send(());
            let _ = thread.join();
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.stop();
    }
}
