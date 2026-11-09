//! Which threads may be deleted, and when. The sidebar is the authority on
//! archive and pin state; the database only supplies activity and protections.

use std::collections::{BTreeMap, HashMap};

use rusqlite::{Connection, OptionalExtension};

use crate::StorageError;
use crate::migrations::backfill::backfill_pending;
use crate::thread_retention::{
    BlockReason, LegacyArchivedPolicy, NANOS_PER_DAY, RetentionInput, RetentionStatus,
    SidebarThread, ThreadRetentionPolicy,
};

const NANOS_PER_SECOND: i64 = 1_000_000_000;

fn days_ns(policy: &ThreadRetentionPolicy) -> i64 {
    i64::from(policy.archived_delete_after_days).saturating_mul(NANOS_PER_DAY)
}

/// The latest activity the sidebar and the database know of for one thread.
fn thread_activity_ns(conn: &Connection, thread: &SidebarThread) -> Result<i64, StorageError> {
    let stored: Option<i64> = conn
        .query_row(
            "SELECT updated_at_ns FROM threads WHERE thread_id = ?1",
            [&thread.thread_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(thread
        .updated_at_s
        .saturating_mul(NANOS_PER_SECOND)
        .max(stored.unwrap_or(0)))
}

/// Latest save of any run bound to the thread.
fn bound_runs_activity_ns(conn: &Connection, thread_id: &str) -> Result<i64, StorageError> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(c.updated_at_ns), 0) FROM thread_runs r \
         JOIN run_contexts c ON c.run_id = r.run_id WHERE r.thread_id = ?1",
        [thread_id],
        |row| row.get(0),
    )?)
}

/// When the thread becomes due. `None` means it already is (a legacy archive
/// under the immediate policy).
pub(super) fn due_at_ns(
    thread: &SidebarThread,
    activity_ns: i64,
    policy: &ThreadRetentionPolicy,
) -> Option<i64> {
    let origin = match (thread.archived_at_s, policy.legacy) {
        (None, LegacyArchivedPolicy::Immediate) => return None,
        (None, LegacyArchivedPolicy::ByLastActivity) => activity_ns,
        (Some(archived_at_s), _) => archived_at_s
            .saturating_mul(NANOS_PER_SECOND)
            .max(activity_ns),
    };
    Some(origin.saturating_add(days_ns(policy)))
}

fn has_eval_sample(conn: &Connection, thread_id: &str) -> Result<bool, StorageError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM eval_samples WHERE thread_id = ?1)",
        [thread_id],
        |row| row.get(0),
    )?)
}

/// Status of every sidebar thread, for display. Only archived, unpinned threads
/// touch the database, so the cost follows the archived count.
pub(crate) fn schedule(
    conn: &Connection,
    input: &RetentionInput,
    policy: &ThreadRetentionPolicy,
    overrides: &BTreeMap<String, BlockReason>,
) -> Result<Vec<(String, RetentionStatus)>, StorageError> {
    let mut statuses = Vec::with_capacity(input.threads.len());
    for thread in &input.threads {
        let status = if !thread.archived {
            RetentionStatus::NotArchived
        } else if policy.archived_delete_after_days == 0 {
            RetentionStatus::Disabled
        } else if thread.pinned {
            RetentionStatus::Blocked(BlockReason::Pinned)
        } else if let Some(reason) = overrides.get(&thread.thread_id) {
            RetentionStatus::Blocked(reason.clone())
        } else if let Some(reason) = unresolved_reason(conn, &thread.thread_id)? {
            RetentionStatus::Blocked(BlockReason::FamilyUnresolved(reason))
        } else if has_eval_sample(conn, &thread.thread_id)? {
            RetentionStatus::Blocked(BlockReason::EvalSample)
        } else {
            let activity = thread_activity_ns(conn, thread)?
                .max(bound_runs_activity_ns(conn, &thread.thread_id)?);
            let own = match due_at_ns(thread, activity, policy) {
                Some(due) if due > input.now_ns => {
                    let days = (due - input.now_ns + NANOS_PER_DAY - 1) / NANOS_PER_DAY;
                    RetentionStatus::DueIn {
                        days: u32::try_from(days).unwrap_or(u32::MAX),
                    }
                }
                _ => RetentionStatus::DueNextStartup,
            };
            // Nothing can be confirmed while the event backfill is unfinished: the start
            // is still preparing, which is not the same as "due".
            let preparing = own == RetentionStatus::DueNextStartup
                && (backfill_pending(conn)? || management_projection_pending(conn)?);
            // A thread is deleted with its family, so the family's state decides.
            if preparing {
                RetentionStatus::Blocked(BlockReason::Exploring)
            } else {
                match family_hold(conn, &thread.thread_id, input, policy, overrides)? {
                    Some(reason) => RetentionStatus::Blocked(reason),
                    None => own,
                }
            }
        };
        statuses.push((thread.thread_id.clone(), status));
    }
    Ok(statuses)
}

/// The identity an exploration could not resolve in the thread's family, if the last
/// exploration found one.
fn unresolved_reason(conn: &Connection, thread_id: &str) -> Result<Option<String>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT reason_id FROM retention_unresolved WHERE thread_id = ?1",
            [thread_id],
            |row| row.get(0),
        )
        .optional()?)
}

/// Why the family a thread was already found to belong to would not be confirmed
/// now, as far as the stored exploration shows. `None` for a thread no exploration
/// has reached yet (nothing is known about its family), and for a family that would
/// be confirmed.
fn family_hold(
    conn: &Connection,
    thread_id: &str,
    input: &RetentionInput,
    policy: &ThreadRetentionPolicy,
    overrides: &BTreeMap<String, BlockReason>,
) -> Result<Option<BlockReason>, StorageError> {
    let key: Option<String> = conn
        .query_row(
            "SELECT family_key FROM retention_staging WHERE kind = 'thread' AND id = ?1 \
             ORDER BY family_key LIMIT 1",
            [thread_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(key) = key else {
        return Ok(None);
    };
    let unfinished: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM retention_staging \
         WHERE family_key = ?1 AND kind IN ('frontier_thread', 'frontier_run'))",
        [&key],
        |row| row.get(0),
    )?;
    if unfinished {
        return Ok(Some(BlockReason::Exploring));
    }
    let members = super::closure::members(conn, &key, "thread")?;
    let owners: BTreeMap<String, String> = overrides
        .iter()
        .filter_map(|(thread, reason)| match reason {
            BlockReason::Owner(text) => Some((thread.clone(), text.clone())),
            _ => None,
        })
        .collect();
    family_blocked(conn, &key, &members, input, policy, &owners)
}

/// Why a fully explored family cannot be confirmed now, if it cannot.
///
/// A member must be known to the sidebar, archived, unpinned, due, without an
/// evaluation sample and not held by an owner. The family is due when its latest
/// activity (any member's, any of its runs') is old enough.
pub(super) fn family_blocked(
    conn: &Connection,
    family_key: &str,
    threads: &[String],
    input: &RetentionInput,
    policy: &ThreadRetentionPolicy,
    owners: &BTreeMap<String, String>,
) -> Result<Option<BlockReason>, StorageError> {
    // Until the sidebar/mirror ACKs a management change, the old and new
    // family are not both knowable from the current relation. Conservatively
    // hold confirmation globally; exploration remains bounded and resumable.
    if management_projection_pending(conn)? {
        return Ok(Some(BlockReason::Exploring));
    }
    let sidebar: HashMap<&str, &SidebarThread> = input
        .threads
        .iter()
        .map(|thread| (thread.thread_id.as_str(), thread))
        .collect();
    let run_activity: i64 = conn.query_row(
        "SELECT COALESCE(MAX(updated_at_ns), 0) FROM run_contexts \
         WHERE run_id IN (SELECT id FROM retention_staging \
                          WHERE family_key = ?1 AND kind = 'run')",
        [family_key],
        |row| row.get(0),
    )?;
    for thread_id in threads {
        let Some(thread) = sidebar.get(thread_id.as_str()) else {
            return Ok(Some(BlockReason::FamilyUnresolved(thread_id.clone())));
        };
        if !thread.archived {
            return Ok(Some(BlockReason::FamilyMemberNotDue(thread_id.clone())));
        }
        if thread.pinned {
            return Ok(Some(BlockReason::Pinned));
        }
        if has_eval_sample(conn, thread_id)? {
            return Ok(Some(BlockReason::EvalSample));
        }
        if let Some(reason) = owners.get(thread_id) {
            return Ok(Some(BlockReason::Owner(reason.clone())));
        }
        let activity = thread_activity_ns(conn, thread)?.max(run_activity);
        if due_at_ns(thread, activity, policy).is_some_and(|due| due > input.now_ns) {
            return Ok(Some(BlockReason::FamilyMemberNotDue(thread_id.clone())));
        }
    }
    Ok(None)
}

fn management_projection_pending(conn: &Connection) -> Result<bool, StorageError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM lanes WHERE management_revision > 1 AND management_projection_revision < management_revision)",
        [], |row| row.get(0),
    )?)
}
