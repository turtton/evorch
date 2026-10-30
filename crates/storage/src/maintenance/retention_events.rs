//! Live, bounded removal of high-volume observational events.
//!
//! Three kinds of rows dominate the event table without being needed to rebuild
//! any state: `Fault/SubscriberLagged`, `Lifecycle/RunProgress` (both are no
//! longer stored; this removes what older versions left behind) and the
//! provider request observations (`RequestStarted`, `RequestCompleted`,
//! `FirstTokenObserved`, `CacheReuseObserved`), which expire after
//! [`StorageConfig::provider_event_retention_days`](crate::StorageConfig).
//!
//! One [`tick`] runs in one immediate transaction and is bounded twice: it
//! deletes at most [`MAX_DELETIONS_PER_TICK`] rows and examines at most
//! [`MAX_SCANNED_PER_TARGET`] candidate rows per target that has to filter
//! (everything except the unconditional `(kind, subkind)` deletes). Targets
//! that filter remember how far they have looked in a [`Cursor`], so a long
//! history is covered over several ticks instead of one long write lock.
//!
//! Rows the event backfill has not converted yet (`subkind IS NULL`) are found
//! through the same `(kind, subkind, id)` index, which stores NULL subkinds
//! next to each other, and confirmed by the inner tag of their payload.
//!
//! Deletion relies on the v12 triggers for `event_session_bytes` and
//! `event_day_bytes`; `sessions.total_event_bytes` is adjusted here like in the
//! other retention paths.

use std::collections::HashMap;

use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use crate::StorageError;

/// Rows deleted by one tick, over all targets.
pub(crate) const MAX_DELETIONS_PER_TICK: usize = 5_000;
/// Candidate rows one filtering target examines per tick.
pub(crate) const MAX_SCANNED_PER_TARGET: usize = 1_000;

const NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;

const PROVIDER_SUBKINDS: &[&str] = &[
    "RequestStarted",
    "RequestCompleted",
    "FirstTokenObserved",
    "CacheReuseObserved",
];

#[derive(Clone, Copy)]
enum Expiry {
    /// Every row of the target is removed.
    Always,
    /// Only rows older than the provider retention window are removed.
    ProviderWindow,
}

struct Target {
    kind: &'static str,
    /// Subkinds removed. An indexed target has exactly one.
    subkinds: &'static [&'static str],
    /// Rows the backfill has not reached, confirmed by their payload tag.
    unconverted: bool,
    expiry: Expiry,
}

const fn target(
    kind: &'static str,
    subkinds: &'static [&'static str],
    unconverted: bool,
    expiry: Expiry,
) -> Target {
    Target {
        kind,
        subkinds,
        unconverted,
        expiry,
    }
}

/// Unconditional indexed deletes first (they drain and never look at a row they
/// keep), then the targets that filter.
const TARGETS: [Target; 9] = [
    target("Fault", &["SubscriberLagged"], false, Expiry::Always),
    target("Lifecycle", &["RunProgress"], false, Expiry::Always),
    target("Fault", &["SubscriberLagged"], true, Expiry::Always),
    target("Lifecycle", &["RunProgress"], true, Expiry::Always),
    target(
        "Provider",
        &["RequestStarted"],
        false,
        Expiry::ProviderWindow,
    ),
    target(
        "Provider",
        &["RequestCompleted"],
        false,
        Expiry::ProviderWindow,
    ),
    target(
        "Provider",
        &["FirstTokenObserved"],
        false,
        Expiry::ProviderWindow,
    ),
    target(
        "Provider",
        &["CacheReuseObserved"],
        false,
        Expiry::ProviderWindow,
    ),
    target("Provider", PROVIDER_SUBKINDS, true, Expiry::ProviderWindow),
];

/// Where each target resumes. Not persisted: a restart begins a new pass.
#[derive(Debug)]
pub(crate) struct Cursor {
    after: [i64; TARGETS.len()],
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            after: [i64::MIN; TARGETS.len()],
        }
    }
}

/// What one tick removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Summary {
    pub(crate) event_count: u64,
    pub(crate) payload_bytes: u64,
}

struct Candidate {
    id: i64,
    session_id: Option<String>,
    payload_bytes: i64,
    wall_clock_ns: i64,
    /// Inner tag taken from the payload; only selected for unconverted rows.
    tag: Option<String>,
}

/// Removes one bounded batch. `now_ns` is the wall clock in nanoseconds since
/// the Unix epoch; `provider_retention_days == 0` keeps every provider
/// observation (the unconditional targets still run).
pub(crate) fn tick(
    conn: &Connection,
    cursor: &mut Cursor,
    now_ns: i64,
    provider_retention_days: u32,
) -> Result<Summary, StorageError> {
    let provider_cutoff = if provider_retention_days == 0 {
        None
    } else {
        i64::from(provider_retention_days)
            .checked_mul(NANOS_PER_DAY)
            .map(|window| now_ns.saturating_sub(window))
    };
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let mut summary = Summary::default();
    let mut sessions: HashMap<String, i64> = HashMap::new();
    for (index, target) in TARGETS.iter().enumerate() {
        let budget = MAX_DELETIONS_PER_TICK.saturating_sub(summary.event_count as usize);
        if budget == 0 {
            break;
        }
        let cutoff = match target.expiry {
            Expiry::Always => None,
            Expiry::ProviderWindow => match provider_cutoff {
                Some(cutoff) => Some(cutoff),
                None => continue,
            },
        };
        run_target(
            &tx,
            target,
            &mut cursor.after[index],
            cutoff,
            budget,
            &mut summary,
            &mut sessions,
        )?;
    }
    for (session, bytes) in sessions {
        tx.execute(
            "UPDATE sessions SET total_event_bytes = MAX(0, total_event_bytes - ?1) WHERE id = ?2",
            params![bytes, session],
        )?;
    }
    tx.commit()?;
    Ok(summary)
}

fn run_target(
    tx: &Transaction<'_>,
    target: &Target,
    after: &mut i64,
    cutoff: Option<i64>,
    budget: usize,
    summary: &mut Summary,
    sessions: &mut HashMap<String, i64>,
) -> Result<(), StorageError> {
    // Unconditional indexed rows are all deleted, so what is examined is
    // bounded by the deletion budget; filtering targets by their scan cap.
    let filtering = target.unconverted || cutoff.is_some();
    let limit = if filtering {
        MAX_SCANNED_PER_TARGET
    } else {
        budget
    };
    let candidates = candidates(tx, target, *after, limit)?;
    let fetched = candidates.len();
    let mut cut_by_budget = false;
    let mut last_processed = None;
    let mut deleted_here = 0_usize;
    for candidate in candidates {
        let delete = cutoff.is_none_or(|cutoff| candidate.wall_clock_ns < cutoff)
            && (!target.unconverted
                || candidate
                    .tag
                    .as_deref()
                    .is_some_and(|tag| target.subkinds.contains(&tag)));
        if delete {
            if deleted_here == budget {
                cut_by_budget = true;
                break;
            }
            tx.execute("DELETE FROM events WHERE id = ?1", [candidate.id])?;
            deleted_here += 1;
            summary.event_count += 1;
            summary.payload_bytes = summary
                .payload_bytes
                .saturating_add(u64::try_from(candidate.payload_bytes).unwrap_or(0));
            if let Some(session) = candidate.session_id {
                *sessions.entry(session).or_default() += candidate.payload_bytes;
            }
        }
        last_processed = Some(candidate.id);
    }
    *after = match last_processed {
        // The index is exhausted: the next tick starts a new pass, which also
        // finds rows that arrived behind the cursor.
        _ if !cut_by_budget && fetched < limit => i64::MIN,
        Some(id) => id,
        None => *after,
    };
    Ok(())
}

/// The inner query reads index entries only and carries the LIMIT, so the
/// number of rows examined is bounded even when most of them are kept.
fn candidate_sql(unconverted: bool) -> String {
    let (filter, tag) = if unconverted {
        (
            "subkind IS NULL",
            "CASE WHEN json_valid(e.payload) THEN json_extract(e.payload, '$.payload.kind') END",
        )
    } else {
        ("subkind = ?4", "NULL")
    };
    format!(
        "SELECT e.id, e.session_id, OCTET_LENGTH(e.payload), e.wall_clock_ns, {tag} \
         FROM (SELECT id FROM events INDEXED BY idx_events_kind_subkind_id \
               WHERE kind = ?1 AND {filter} AND id > ?2 ORDER BY id LIMIT ?3) s \
         JOIN events e ON e.id = s.id ORDER BY s.id"
    )
}

fn candidates(
    tx: &Transaction<'_>,
    target: &Target,
    after: i64,
    limit: usize,
) -> Result<Vec<Candidate>, StorageError> {
    let sql = candidate_sql(target.unconverted);
    let mut statement = tx.prepare_cached(&sql)?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let map = |row: &rusqlite::Row<'_>| {
        Ok(Candidate {
            id: row.get(0)?,
            session_id: row.get(1)?,
            payload_bytes: row.get(2)?,
            wall_clock_ns: row.get(3)?,
            tag: row.get(4)?,
        })
    };
    let rows = if target.unconverted {
        statement.query_map(params![target.kind, after, limit], map)?
    } else {
        statement.query_map(params![target.kind, after, limit, target.subkinds[0]], map)?
    };
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

#[cfg(test)]
#[path = "retention_events_tests.rs"]
mod tests;
