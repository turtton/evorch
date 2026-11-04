//! Inheritance: a derived thread's pending right to take its parent's link.

use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

use super::{
    active_link, check_id, enum_at, immediate, key_at, lifecycle, link_by_id, replace_active_link,
    snapshots, tombstone_check,
};
use crate::StorageError;
use crate::pull_requests::{
    InheritancePending, InheritanceReason, InheritanceRecord, InheritanceResolution,
    InheritanceSkipReason, InheritanceState, LinkOrigin, NewPullRequestInheritance,
    PULL_REQUEST_ERROR_MAX_CHARS, PULL_REQUEST_LIST_MAX, PullRequestRejection, PullRequestState,
    sanitize_single_line,
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
    tombstone_check(&tx, &new.child_thread_id)?;
    if let Some(existing) = get(&tx, &new.child_thread_id)? {
        return Ok(existing);
    }
    let parent = link_by_id(&tx, new.parent_link_id)?.ok_or(StorageError::PullRequest(
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
    if is_lane(&tx, &new.child_thread_id)? {
        return Err(StorageError::PullRequest(PullRequestRejection::LaneThread));
    }
    let (expected, _) = lifecycle(&tx, &new.child_thread_id)?;
    tx.execute(
        "INSERT INTO thread_pull_request_inheritance (child_thread_id, parent_thread_id, \
            parent_link_id, provider, host, repository, number, expected_child_revision, state, \
            created_at_ns) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?9)",
        params![
            new.child_thread_id,
            parent.thread_id,
            parent.link_id,
            parent.key.provider.as_str(),
            parent.key.host,
            parent.key.repository,
            parent.key.number,
            expected,
            new.at_ns,
        ],
    )?;
    let record = get(&tx, &new.child_thread_id)?
        .ok_or(StorageError::OutOfRange("pull request inheritance"))?;
    tx.commit()?;
    Ok(record)
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
    limit: usize,
) -> Result<Vec<InheritanceRecord>, StorageError> {
    let limit = i64::try_from(limit.clamp(1, PULL_REQUEST_LIST_MAX)).unwrap_or(i64::MAX);
    let mut statement = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM thread_pull_request_inheritance \
         WHERE state = 'pending' \
           AND child_thread_id NOT IN (SELECT thread_id FROM thread_tombstones) \
         ORDER BY created_at_ns, child_thread_id LIMIT ?1"
    ))?;
    Ok(statement
        .query_map([limit], from_row)?
        .collect::<Result<_, _>>()?)
}
