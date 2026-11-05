//! Inheritance: a derived thread's pending right to take its parent's link.

use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

use super::{
    active_link, check_id, enum_at, immediate, key_at, lifecycle, link_by_id, replace_active_link,
    snapshots, tombstone_check,
};
use crate::StorageError;
use crate::pull_requests::{
    InheritanceCursor, InheritancePending, InheritanceReason, InheritanceRecord,
    InheritanceResolution, InheritanceSkipReason, InheritanceState, LinkOrigin,
    NewPullRequestInheritance, PULL_REQUEST_ERROR_MAX_CHARS, PULL_REQUEST_LIST_MAX,
    PullRequestRejection, PullRequestState, sanitize_single_line,
};

const COLUMNS: &str = "child_thread_id, parent_thread_id, parent_link_id, provider, host, \
    repository, number, expected_child_revision, state, decision_reason, applied_link_id, \
    attempt_count, last_attempt_at_ns, last_attempt_note, created_at_ns, decided_at_ns";

fn from_row(row: &Row<'_>) -> rusqlite::Result<InheritanceRecord> {
    let reason: Option<String> = row.get(9)?;
    let decision_reason = match reason {
        None => None,
        Some(label) => Some(InheritanceReason::parse(&label).ok_or_else(|| {
            rusqlite::Error::InvalidColumnType(
                9,
                "decision reason".into(),
                rusqlite::types::Type::Text,
            )
        })?),
    };
    Ok(InheritanceRecord {
        child_thread_id: row.get(0)?,
        parent_thread_id: row.get(1)?,
        parent_link_id: row.get(2)?,
        key: key_at(row, 3)?,
        expected_child_revision: row.get(7)?,
        state: enum_at(row, 8, "inheritance state", InheritanceState::parse)?,
        decision_reason,
        applied_link_id: row.get(10)?,
        attempt_count: row.get(11)?,
        last_attempt_at_ns: row.get(12)?,
        last_attempt_note: row.get(13)?,
        created_at_ns: row.get(14)?,
        decided_at_ns: row.get(15)?,
    })
}

pub(crate) fn get(
    conn: &Connection,
    child_thread_id: &str,
) -> Result<Option<InheritanceRecord>, StorageError> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM thread_pull_request_inheritance WHERE child_thread_id = ?1"
            ),
            [child_thread_id],
            from_row,
        )
        .optional()?)
}

/// Whether the thread has a pending inheritance row (as a child).
pub(crate) fn has_pending(conn: &Connection, child_thread_id: &str) -> Result<bool, StorageError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM thread_pull_request_inheritance \
         WHERE child_thread_id = ?1 AND state = 'pending')",
        [child_thread_id],
        |row| row.get(0),
    )?)
}

/// The explicit decision the thread already made about pull requests, if any: an explicit
/// link row (an unlinked one counts as the stronger "unlinked"). An inherited link is not
/// a decision of the thread.
fn explicit_decision(
    conn: &Connection,
    thread_id: &str,
) -> Result<Option<InheritanceReason>, StorageError> {
    let (unlinked, linked): (bool, bool) = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM thread_pull_requests WHERE thread_id = ?1 \
                       AND origin = 'explicit' AND end_reason = 'unlinked'), \
                EXISTS(SELECT 1 FROM thread_pull_requests WHERE thread_id = ?1 \
                       AND origin = 'explicit')",
        [thread_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(if unlinked {
        Some(InheritanceReason::ExplicitUnlink)
    } else if linked {
        Some(InheritanceReason::ExplicitLink)
    } else {
        None
    })
}

fn is_lane(conn: &Connection, thread_id: &str) -> Result<bool, StorageError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM threads WHERE thread_id = ?1 AND lineage_kind = 'lane')",
        [thread_id],
        |row| row.get(0),
    )?)
}

pub(crate) fn schedule(
    conn: &Connection,
    new: &NewPullRequestInheritance,
) -> Result<InheritanceRecord, StorageError> {
    check_id("child_thread_id", &new.child_thread_id)?;
    let tx = immediate(conn)?;
    let record = schedule_in(&tx, new)?;
    tx.commit()?;
    Ok(record)
}

/// Pins whatever link `parent_thread_id` has *in this transaction*, so a parent that relinks
/// at the same time is pinned to the old link or the new one, never to neither. `None` when the
/// parent has no active link. An existing row of the child is returned as it is.
pub(crate) fn schedule_from_parent(
    conn: &Connection,
    child_thread_id: &str,
    parent_thread_id: &str,
    at_ns: i64,
) -> Result<Option<InheritanceRecord>, StorageError> {
    check_id("child_thread_id", child_thread_id)?;
    check_id("parent_thread_id", parent_thread_id)?;
    let tx = immediate(conn)?;
    tombstone_check(&tx, child_thread_id)?;
    if let Some(existing) = get(&tx, child_thread_id)? {
        return Ok(Some(existing));
    }
    let active: Option<u64> = tx
        .query_row(
            "SELECT link_id FROM thread_pull_requests \
             WHERE thread_id = ?1 AND superseded_at_ns IS NULL",
            [parent_thread_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(parent_link_id) = active else {
        return Ok(None);
    };
    let record = schedule_in(
        &tx,
        &NewPullRequestInheritance {
            child_thread_id: child_thread_id.to_owned(),
            parent_link_id,
            at_ns,
        },
    )?;
    tx.commit()?;
    Ok(Some(record))
}

fn schedule_in(
    tx: &Transaction<'_>,
    new: &NewPullRequestInheritance,
) -> Result<InheritanceRecord, StorageError> {
    tombstone_check(tx, &new.child_thread_id)?;
    if let Some(existing) = get(tx, &new.child_thread_id)? {
        return Ok(existing);
    }
    let parent = link_by_id(tx, new.parent_link_id)?.ok_or(StorageError::PullRequest(
        PullRequestRejection::ParentLinkNotFound,
    ))?;
    if parent.end.is_some() {
        return Err(StorageError::PullRequest(
            PullRequestRejection::ParentLinkNotActive,
        ));
    }
    if parent.thread_id == new.child_thread_id {
        return Err(StorageError::PullRequest(PullRequestRejection::SameThread));
    }
    if is_lane(tx, &new.child_thread_id)? {
        return Err(StorageError::PullRequest(PullRequestRejection::LaneThread));
    }
    let (expected, _) = lifecycle(tx, &new.child_thread_id)?;
    // A child that already made its own decision (a link or an unlink) before this was
    // scheduled does not get the parent's link: the row is decided at once, so the
    // decision is on record and no retry can apply it.
    let earlier_decision = if expected > 0 {
        Some(
            explicit_decision(tx, &new.child_thread_id)?.unwrap_or(InheritanceReason::ChildChanged),
        )
    } else {
        explicit_decision(tx, &new.child_thread_id)?
    };
    let (state, reason, decided_at) = match earlier_decision {
        Some(reason) => (
            InheritanceState::Cancelled,
            Some(reason.as_str()),
            Some(new.at_ns),
        ),
        None => (InheritanceState::Pending, None, None),
    };
    tx.execute(
        "INSERT INTO thread_pull_request_inheritance (child_thread_id, parent_thread_id, \
            parent_link_id, provider, host, repository, number, expected_child_revision, state, \
            decision_reason, created_at_ns, decided_at_ns) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            new.child_thread_id,
            parent.thread_id,
            parent.link_id,
            parent.key.provider.as_str(),
            parent.key.host,
            parent.key.repository,
            parent.key.number,
            expected,
            state.as_str(),
            reason,
            new.at_ns,
            decided_at,
        ],
    )?;
    get(tx, &new.child_thread_id)?.ok_or(StorageError::OutOfRange("pull request inheritance"))
}

fn decide(
    tx: &Transaction<'_>,
    child: &str,
    state: InheritanceState,
    reason: InheritanceReason,
    at_ns: i64,
) -> Result<(), StorageError> {
    tx.execute(
        "UPDATE thread_pull_request_inheritance SET state = ?2, decision_reason = ?3, \
            decided_at_ns = ?4 WHERE child_thread_id = ?1 AND state = 'pending'",
        params![child, state.as_str(), reason.as_str(), at_ns],
    )?;
    Ok(())
}

fn note_attempt(
    tx: &Transaction<'_>,
    child: &str,
    note: &str,
    at_ns: i64,
) -> Result<(), StorageError> {
    let note = sanitize_single_line(
        &secret_guard::SecretRedactor::from_env().redact(note).text,
        PULL_REQUEST_ERROR_MAX_CHARS,
    );
    tx.execute(
        "UPDATE thread_pull_request_inheritance SET attempt_count = attempt_count + 1, \
            last_attempt_at_ns = ?2, last_attempt_note = ?3 \
         WHERE child_thread_id = ?1 AND state = 'pending'",
        params![child, at_ns, note],
    )?;
    Ok(())
}

pub(crate) fn resolve(
    conn: &Connection,
    child: &str,
    required_success_seq: u64,
    at_ns: i64,
) -> Result<InheritanceResolution, StorageError> {
    check_id("child_thread_id", child)?;
    let tx = immediate(conn)?;
    tombstone_check(&tx, child)?;
    let Some(row) = get(&tx, child)? else {
        return Ok(InheritanceResolution::Missing);
    };
    if row.state != InheritanceState::Pending {
        return Ok(InheritanceResolution::AlreadyDecided(row.state));
    }
    if is_lane(&tx, child)? {
        decide(
            &tx,
            child,
            InheritanceState::Skipped,
            InheritanceReason::LaneThread,
            at_ns,
        )?;
        tx.commit()?;
        return Ok(InheritanceResolution::Skipped(
            InheritanceReason::LaneThread,
        ));
    }
    if let Some(reason) = explicit_decision(&tx, child)? {
        decide(&tx, child, InheritanceState::Cancelled, reason, at_ns)?;
        tx.commit()?;
        return Ok(InheritanceResolution::Cancelled(reason));
    }
    let (revision, _) = lifecycle(&tx, child)?;
    if revision != row.expected_child_revision {
        decide(
            &tx,
            child,
            InheritanceState::Cancelled,
            InheritanceReason::ChildChanged,
            at_ns,
        )?;
        tx.commit()?;
        return Ok(InheritanceResolution::Cancelled(
            InheritanceReason::ChildChanged,
        ));
    }
    if active_link(&tx, child)?.is_some() {
        decide(
            &tx,
            child,
            InheritanceState::Skipped,
            InheritanceReason::ChildLinked,
            at_ns,
        )?;
        tx.commit()?;
        return Ok(InheritanceResolution::Skipped(
            InheritanceReason::ChildLinked,
        ));
    }
    let snapshot = snapshots::get(&tx, &row.key)?;
    let Some(facts) = snapshot.as_ref().and_then(|s| s.facts.as_ref()) else {
        note_attempt(&tx, child, "pull request state is not known yet", at_ns)?;
        tx.commit()?;
        return Ok(InheritanceResolution::Pending(InheritancePending::NoFacts));
    };
    let success_seq = snapshot.as_ref().map_or(0, |s| s.success_seq);
    if success_seq < required_success_seq {
        note_attempt(
            &tx,
            child,
            "waiting for a fresher pull request state",
            at_ns,
        )?;
        tx.commit()?;
        return Ok(InheritanceResolution::Pending(
            InheritancePending::NeedsFresherSnapshot,
        ));
    }
    let reason = match facts.state {
        PullRequestState::Open => None,
        PullRequestState::Merged => Some(InheritanceReason::Merged),
        PullRequestState::Closed => Some(InheritanceReason::Closed),
    };
    if let Some(reason) = reason {
        decide(&tx, child, InheritanceState::Skipped, reason, at_ns)?;
        tx.commit()?;
        return Ok(InheritanceResolution::Skipped(reason));
    }
    let (link, _, lifecycle_revision) = replace_active_link(
        &tx,
        child,
        &row.key,
        LinkOrigin::Inherited,
        Some(row.parent_link_id),
        at_ns,
    )?;
    tx.execute(
        "UPDATE thread_pull_request_inheritance SET state = 'applied', applied_link_id = ?2, \
            decided_at_ns = ?3 WHERE child_thread_id = ?1 AND state = 'pending'",
        params![child, link.link_id, at_ns],
    )?;
    tx.commit()?;
    Ok(InheritanceResolution::Applied {
        link,
        lifecycle_revision,
    })
}

pub(crate) fn record_attempt(
    conn: &Connection,
    child: &str,
    note: &str,
    at_ns: i64,
) -> Result<Option<InheritanceRecord>, StorageError> {
    check_id("child_thread_id", child)?;
    let tx = immediate(conn)?;
    tombstone_check(&tx, child)?;
    note_attempt(&tx, child, note, at_ns)?;
    let record = get(&tx, child)?;
    tx.commit()?;
    Ok(record)
}

pub(crate) fn skip(
    conn: &Connection,
    child: &str,
    reason: InheritanceSkipReason,
    at_ns: i64,
) -> Result<Option<InheritanceRecord>, StorageError> {
    check_id("child_thread_id", child)?;
    let tx = immediate(conn)?;
    tombstone_check(&tx, child)?;
    decide(
        &tx,
        child,
        InheritanceState::Skipped,
        reason.reason(),
        at_ns,
    )?;
    let record = get(&tx, child)?;
    tx.commit()?;
    Ok(record)
}

pub(crate) fn pending(
    conn: &Connection,
    after: Option<InheritanceCursor>,
    limit: usize,
) -> Result<Vec<InheritanceRecord>, StorageError> {
    let limit = i64::try_from(limit.clamp(1, PULL_REQUEST_LIST_MAX)).unwrap_or(i64::MAX);
    let (after_ns, after_child) = match after {
        Some(cursor) => (Some(cursor.created_at_ns), Some(cursor.child_thread_id)),
        None => (None, None),
    };
    let mut statement = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM thread_pull_request_inheritance \
         WHERE state = 'pending' \
           AND (?2 IS NULL OR (created_at_ns, child_thread_id) > (?2, ?3)) \
           AND child_thread_id NOT IN (SELECT thread_id FROM thread_tombstones) \
         ORDER BY created_at_ns, child_thread_id LIMIT ?1"
    ))?;
    Ok(statement
        .query_map(params![limit, after_ns, after_child], from_row)?
        .collect::<Result<_, _>>()?)
}
