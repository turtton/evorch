//! SQL of the pull request integration storage (see [`crate::pull_requests`] for the
//! contract): links, lifecycle state, notification keys and candidates. Snapshots and
//! inheritance are in the submodules.

use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::StorageError;
use crate::pull_requests::{
    CandidatePresented, CandidateState, InheritanceReason, LinkEnd, LinkEndReason, LinkOrigin,
    LinkPullRequest, LinkReceipt, LocalHeadObservation, NewPullRequestCandidate, NotificationKey,
    PULL_REQUEST_CANDIDATES_MAX, PULL_REQUEST_ID_MAX_BYTES, PULL_REQUEST_LIST_MAX,
    PULL_REQUEST_NOTIFICATION_KIND_MAX_BYTES, PULL_REQUEST_NOTIFIED_MAX, PullRequestCandidate,
    PullRequestKey, PullRequestLink, PullRequestProvider, PullRequestRejection,
    ThreadPullRequestState, UnlinkReceipt, invalid,
};

pub(crate) mod inheritance;
pub(crate) mod snapshots;

const LINK_COLUMNS: &str = "link_id, thread_id, provider, host, repository, number, origin, \
    parent_link_id, created_at_ns, link_revision, superseded_at_ns, end_reason, end_revision";

const CANDIDATE_COLUMNS: &str = "candidate_id, thread_id, provider, host, repository, number, \
    source_run_id, source_call_id, state, created_at_ns, updated_at_ns";

const STALE_KIND: &str = "pull_request_lifecycle";

pub(super) fn immediate(conn: &Connection) -> Result<Transaction<'_>, StorageError> {
    Ok(Transaction::new_unchecked(
        conn,
        TransactionBehavior::Immediate,
    )?)
}

pub(super) fn check_id(field: &'static str, value: &str) -> Result<(), StorageError> {
    if value.trim().is_empty() {
        return Err(invalid(field, "must not be empty"));
    }
    if value.len() > PULL_REQUEST_ID_MAX_BYTES {
        return Err(invalid(field, "is too long"));
    }
    Ok(())
}

/// Refuses a thread that retention deleted.
pub(super) fn tombstone_check(conn: &Connection, thread_id: &str) -> Result<(), StorageError> {
    let hit: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM thread_tombstones WHERE thread_id = ?1)",
        [thread_id],
        |row| row.get(0),
    )?;
    if hit {
        return Err(StorageError::Tombstoned {
            run_id: None,
            thread_id: Some(thread_id.to_owned()),
        });
    }
    Ok(())
}

fn column_error(index: usize, name: &'static str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        rusqlite::types::Type::Text,
        format!("unknown pull request {name}").into(),
    )
}

/// Decodes `(provider, host, repository, number)` that start at column `offset`.
pub(super) fn key_at(row: &Row<'_>, offset: usize) -> rusqlite::Result<PullRequestKey> {
    let provider: String = row.get(offset)?;
    Ok(PullRequestKey {
        provider: PullRequestProvider::parse(&provider)
            .ok_or_else(|| column_error(offset, "provider"))?,
        host: row.get(offset + 1)?,
        repository: row.get(offset + 2)?,
        number: row.get(offset + 3)?,
    })
}

pub(super) fn enum_at<T>(
    row: &Row<'_>,
    index: usize,
    name: &'static str,
    parse: impl Fn(&str) -> Option<T>,
) -> rusqlite::Result<T> {
    let label: String = row.get(index)?;
    parse(&label).ok_or_else(|| column_error(index, name))
}

fn link_from_row(row: &Row<'_>) -> rusqlite::Result<PullRequestLink> {
    let superseded: Option<i64> = row.get(10)?;
    let end = match superseded {
        None => None,
        Some(at_ns) => Some(LinkEnd {
            at_ns,
            reason: enum_at(row, 11, "end reason", LinkEndReason::parse)?,
            revision: row.get(12)?,
        }),
    };
    Ok(PullRequestLink {
        link_id: row.get(0)?,
        thread_id: row.get(1)?,
        key: key_at(row, 2)?,
        origin: enum_at(row, 6, "origin", LinkOrigin::parse)?,
        parent_link_id: row.get(7)?,
        created_at_ns: row.get(8)?,
        link_revision: row.get(9)?,
        end,
    })
}

fn candidate_from_row(row: &Row<'_>) -> rusqlite::Result<PullRequestCandidate> {
    Ok(PullRequestCandidate {
        candidate_id: row.get(0)?,
        thread_id: row.get(1)?,
        key: key_at(row, 2)?,
        source_run_id: row.get(6)?,
        source_call_id: row.get(7)?,
        state: enum_at(row, 8, "candidate state", CandidateState::parse)?,
        created_at_ns: row.get(9)?,
        updated_at_ns: row.get(10)?,
    })
}

pub(super) fn link_by_id(
    conn: &Connection,
    link_id: u64,
) -> Result<Option<PullRequestLink>, StorageError> {
    Ok(conn
        .query_row(
            &format!("SELECT {LINK_COLUMNS} FROM thread_pull_requests WHERE link_id = ?1"),
            [link_id],
            link_from_row,
        )
        .optional()?)
}

pub(super) fn active_link(
    conn: &Connection,
    thread_id: &str,
) -> Result<Option<PullRequestLink>, StorageError> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {LINK_COLUMNS} FROM thread_pull_requests \
                 WHERE thread_id = ?1 AND superseded_at_ns IS NULL"
            ),
            [thread_id],
            link_from_row,
        )
        .optional()?)
}

/// `(lifecycle_revision, transition_seq)` of a thread; `(0, 0)` before its first decision.
pub(super) fn lifecycle(conn: &Connection, thread_id: &str) -> Result<(u64, u64), StorageError> {
    Ok(conn
        .query_row(
            "SELECT lifecycle_revision, transition_seq FROM thread_pull_request_state \
             WHERE thread_id = ?1",
            [thread_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .unwrap_or((0, 0)))
}

fn stale(expected: u64, current: u64) -> StorageError {
    StorageError::StaleRevision {
        kind: STALE_KIND,
        expected,
        current,
    }
}

/// Advances the thread's lifecycle revision (creating its state row) and forgets the
/// local HEAD observation, which belonged to the previous revision.
fn bump_revision(tx: &Transaction<'_>, thread_id: &str) -> Result<u64, StorageError> {
    tx.execute(
        "INSERT OR IGNORE INTO thread_pull_request_state(thread_id) VALUES (?1)",
        [thread_id],
    )?;
    Ok(tx.query_row(
        "UPDATE thread_pull_request_state SET lifecycle_revision = lifecycle_revision + 1, \
            local_head_link_revision = NULL, local_head_sha = NULL, local_head_observed_at_ns = NULL \
         WHERE thread_id = ?1 RETURNING lifecycle_revision",
        [thread_id],
        |row| row.get(0),
    )?)
}

/// Ends the active link (if any) as relinked and creates the new active one, at a new
/// lifecycle revision. The caller has checked the expected revision.
pub(super) fn replace_active_link(
    tx: &Transaction<'_>,
    thread_id: &str,
    key: &PullRequestKey,
    origin: LinkOrigin,
    parent_link_id: Option<u64>,
    at_ns: i64,
) -> Result<(PullRequestLink, Option<PullRequestLink>, u64), StorageError> {
    let previous = active_link(tx, thread_id)?;
    let revision = bump_revision(tx, thread_id)?;
    let superseded = match previous {
        Some(previous) => {
            tx.execute(
                "UPDATE thread_pull_requests SET superseded_at_ns = ?2, end_reason = 'relinked', \
                    end_revision = ?3 WHERE link_id = ?1",
                params![previous.link_id, at_ns, revision],
            )?;
            link_by_id(tx, previous.link_id)?
        }
        None => None,
    };
    tx.execute(
        "INSERT INTO thread_pull_requests (thread_id, provider, host, repository, number, origin, \
            parent_link_id, created_at_ns, link_revision) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            thread_id,
            key.provider.as_str(),
            key.host,
            key.repository,
            key.number,
            origin.as_str(),
            parent_link_id,
            at_ns,
            revision,
        ],
    )?;
    let link_id = u64::try_from(tx.last_insert_rowid())
        .map_err(|_| StorageError::OutOfRange("pull request link id"))?;
    let link = link_by_id(tx, link_id)?.ok_or(StorageError::OutOfRange("pull request link"))?;
    Ok((link, superseded, revision))
}

/// Cancels the thread's own pending inheritance. `true` when one was pending.
pub(super) fn cancel_own_inheritance(
    tx: &Transaction<'_>,
    thread_id: &str,
    reason: InheritanceReason,
    at_ns: i64,
) -> Result<bool, StorageError> {
    let changed = tx.execute(
        "UPDATE thread_pull_request_inheritance SET state = 'cancelled', decision_reason = ?2, \
            decided_at_ns = ?3 WHERE child_thread_id = ?1 AND state = 'pending'",
        params![thread_id, reason.as_str(), at_ns],
    )?;
    Ok(changed > 0)
}

pub(crate) fn link(
    conn: &Connection,
    request: &LinkPullRequest,
) -> Result<LinkReceipt, StorageError> {
    check_id("thread_id", &request.thread_id)?;
    let key = request.key.normalized()?;
    let tx = immediate(conn)?;
    tombstone_check(&tx, &request.thread_id)?;
    let (current, _) = lifecycle(&tx, &request.thread_id)?;
    if current != request.expected_revision {
        return Err(stale(request.expected_revision, current));
    }
    if let Some(active) = active_link(&tx, &request.thread_id)?
        && active.key == key
        && active.origin == LinkOrigin::Explicit
    {
        return Ok(LinkReceipt {
            link: active,
            lifecycle_revision: current,
            changed: false,
            superseded: None,
            cancelled_inheritance: false,
        });
    }
    let (link, superseded, revision) = replace_active_link(
        &tx,
        &request.thread_id,
        &key,
        LinkOrigin::Explicit,
        None,
        request.at_ns,
    )?;
    let cancelled_inheritance = cancel_own_inheritance(
        &tx,
        &request.thread_id,
        InheritanceReason::ExplicitLink,
        request.at_ns,
    )?;
    tx.execute(
        "UPDATE thread_pull_request_candidates SET state = 'confirmed', updated_at_ns = ?6 \
         WHERE thread_id = ?1 AND provider = ?2 AND host = ?3 AND repository = ?4 AND number = ?5 \
           AND state <> 'confirmed'",
        params![
            request.thread_id,
            key.provider.as_str(),
            key.host,
            key.repository,
            key.number,
            request.at_ns,
        ],
    )?;
    tx.commit()?;
    Ok(LinkReceipt {
        link,
        lifecycle_revision: revision,
        changed: true,
        superseded,
        cancelled_inheritance,
    })
}

pub(crate) fn unlink(
    conn: &Connection,
    thread_id: &str,
    expected_revision: u64,
    at_ns: i64,
) -> Result<UnlinkReceipt, StorageError> {
    check_id("thread_id", thread_id)?;
    let tx = immediate(conn)?;
    tombstone_check(&tx, thread_id)?;
    let (current, _) = lifecycle(&tx, thread_id)?;
    if current != expected_revision {
        return Err(stale(expected_revision, current));
    }
    let active = active_link(&tx, thread_id)?;
    // A thread that only holds a pending inheritance has nothing to end, but the person can
    // still decline it: that is an explicit decision and is recorded like any other.
    if active.is_none() && !inheritance::has_pending(&tx, thread_id)? {
        return Err(StorageError::PullRequest(PullRequestRejection::NotLinked));
    }
    let revision = bump_revision(&tx, thread_id)?;
    let ended = match active {
        Some(active) => {
            tx.execute(
                "UPDATE thread_pull_requests SET superseded_at_ns = ?2, end_reason = 'unlinked', \
                    end_revision = ?3 WHERE link_id = ?1",
                params![active.link_id, at_ns, revision],
            )?;
            Some(
                link_by_id(&tx, active.link_id)?
                    .ok_or(StorageError::OutOfRange("pull request link"))?,
            )
        }
        None => None,
    };
    let cancelled_inheritance =
        cancel_own_inheritance(&tx, thread_id, InheritanceReason::ExplicitUnlink, at_ns)?;
    tx.commit()?;
    Ok(UnlinkReceipt {
        ended,
        lifecycle_revision: revision,
        cancelled_inheritance,
    })
}

pub(crate) fn thread_state(
    conn: &Connection,
    thread_id: &str,
) -> Result<ThreadPullRequestState, StorageError> {
    check_id("thread_id", thread_id)?;
    let tx = conn.unchecked_transaction()?;
    let (lifecycle_revision, transition_seq) = lifecycle(&tx, thread_id)?;
    let active = active_link(&tx, thread_id)?;
    let head: Option<(u64, String, i64)> = tx
        .query_row(
            "SELECT local_head_link_revision, local_head_sha, local_head_observed_at_ns \
             FROM thread_pull_request_state WHERE thread_id = ?1 \
               AND local_head_sha IS NOT NULL",
            [thread_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let local_head = match (&active, head) {
        (Some(active), Some((link_revision, head_sha, observed_at_ns)))
            if active.link_revision == link_revision =>
        {
            Some(LocalHeadObservation {
                link_revision,
                head_sha,
                observed_at_ns,
            })
        }
        _ => None,
    };
    let inheritance = inheritance::get(&tx, thread_id)?;
    Ok(ThreadPullRequestState {
        thread_id: thread_id.to_owned(),
        lifecycle_revision,
        transition_seq,
        active_link: active,
        local_head,
        inheritance,
    })
}

pub(crate) fn link_history(
    conn: &Connection,
    thread_id: &str,
    limit: usize,
) -> Result<Vec<PullRequestLink>, StorageError> {
    check_id("thread_id", thread_id)?;
    let limit = i64::try_from(limit.clamp(1, PULL_REQUEST_LIST_MAX)).unwrap_or(i64::MAX);
    let mut statement = conn.prepare(&format!(
        "SELECT {LINK_COLUMNS} FROM thread_pull_requests WHERE thread_id = ?1 \
         ORDER BY link_id DESC LIMIT ?2"
    ))?;
    Ok(statement
        .query_map(params![thread_id, limit], link_from_row)?
        .collect::<Result<_, _>>()?)
}

pub(crate) fn active_links(
    conn: &Connection,
    after_link_id: u64,
    limit: usize,
) -> Result<Vec<PullRequestLink>, StorageError> {
    let limit = i64::try_from(limit.clamp(1, PULL_REQUEST_LIST_MAX)).unwrap_or(i64::MAX);
    let mut statement = conn.prepare(&format!(
        "SELECT {LINK_COLUMNS} FROM thread_pull_requests AS l \
         WHERE superseded_at_ns IS NULL AND link_id > ?1 \
           AND thread_id NOT IN (SELECT thread_id FROM thread_tombstones) \
           AND NOT EXISTS (SELECT 1 FROM threads AS t WHERE t.thread_id = l.thread_id \
                           AND t.archived = 1) \
         ORDER BY link_id LIMIT ?2"
    ))?;
    Ok(statement
        .query_map(params![after_link_id, limit], link_from_row)?
        .collect::<Result<_, _>>()?)
}

fn valid_commit(field: &'static str, sha: &str) -> Result<String, StorageError> {
    let sha = sha.trim().to_ascii_lowercase();
    if !matches!(sha.len(), 40 | 64) || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid(field, "must be a 40 or 64 digit hex commit id"));
    }
    Ok(sha)
}

pub(super) fn valid_commit_id(field: &'static str, sha: &str) -> Result<String, StorageError> {
    valid_commit(field, sha)
}

/// The active link the caller's `link_revision` names, or why it is gone.
fn bound_link(
    tx: &Transaction<'_>,
    thread_id: &str,
    link_revision: u64,
) -> Result<PullRequestLink, StorageError> {
    match active_link(tx, thread_id)? {
        None => Err(StorageError::PullRequest(PullRequestRejection::NotLinked)),
        Some(active) if active.link_revision != link_revision => Err(StorageError::PullRequest(
            PullRequestRejection::LinkNotActive,
        )),
        Some(active) => Ok(active),
    }
}

pub(crate) fn record_local_head(
    conn: &Connection,
    thread_id: &str,
    link_revision: u64,
    head_sha: &str,
    observed_at_ns: i64,
) -> Result<bool, StorageError> {
    check_id("thread_id", thread_id)?;
    let head_sha = valid_commit("head_sha", head_sha)?;
    let tx = immediate(conn)?;
    tombstone_check(&tx, thread_id)?;
    bound_link(&tx, thread_id, link_revision)?;
    let changed = tx.execute(
        "UPDATE thread_pull_request_state SET local_head_link_revision = ?2, local_head_sha = ?3, \
            local_head_observed_at_ns = ?4 \
         WHERE thread_id = ?1 AND (local_head_observed_at_ns IS NULL \
            OR local_head_link_revision <> ?2 OR local_head_observed_at_ns <= ?4)",
        params![thread_id, link_revision, head_sha, observed_at_ns],
    )?;
    tx.commit()?;
    Ok(changed > 0)
}

pub(crate) fn clear_local_head(
    conn: &Connection,
    thread_id: &str,
    link_revision: u64,
) -> Result<bool, StorageError> {
    check_id("thread_id", thread_id)?;
    let tx = immediate(conn)?;
    tombstone_check(&tx, thread_id)?;
    bound_link(&tx, thread_id, link_revision)?;
    let changed = tx.execute(
        "UPDATE thread_pull_request_state SET local_head_link_revision = NULL, \
            local_head_sha = NULL, local_head_observed_at_ns = NULL \
         WHERE thread_id = ?1 AND local_head_link_revision = ?2 AND local_head_sha IS NOT NULL",
        params![thread_id, link_revision],
    )?;
    tx.commit()?;
    Ok(changed > 0)
}

pub(crate) fn next_transition(
    conn: &Connection,
    thread_id: &str,
    link_revision: u64,
) -> Result<u64, StorageError> {
    check_id("thread_id", thread_id)?;
    let tx = immediate(conn)?;
    tombstone_check(&tx, thread_id)?;
    bound_link(&tx, thread_id, link_revision)?;
    let seq = tx.query_row(
        "UPDATE thread_pull_request_state SET transition_seq = transition_seq + 1 \
         WHERE thread_id = ?1 RETURNING transition_seq",
        [thread_id],
        |row| row.get(0),
    )?;
    tx.commit()?;
    Ok(seq)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Notified {
    link_revision: u64,
    transition_seq: u64,
    kind: String,
}

fn check_notification_key(key: &NotificationKey) -> Result<(), StorageError> {
    let kind_ok = !key.kind.is_empty()
        && key.kind.len() <= PULL_REQUEST_NOTIFICATION_KIND_MAX_BYTES
        && key
            .kind
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if !kind_ok {
        return Err(invalid("kind", "must be 1..=48 of a-z, 0-9 and _"));
    }
    if key.link_revision == 0 || key.transition_seq == 0 {
        return Err(invalid("notification_key", "revisions start at 1"));
    }
    Ok(())
}

fn notified_state(
    conn: &Connection,
    thread_id: &str,
) -> Result<(Vec<Notified>, u64, u64), StorageError> {
    let row: Option<(String, u64, u64)> = conn
        .query_row(
            "SELECT notified_json, notified_floor_seq, transition_seq \
             FROM thread_pull_request_state WHERE thread_id = ?1",
            [thread_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((json, floor, transition_seq)) = row else {
        return Ok((Vec::new(), 0, 0));
    };
    let list = serde_json::from_str(&json)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    Ok((list, floor, transition_seq))
}

fn is_claimed(list: &[Notified], floor: u64, key: &NotificationKey) -> bool {
    key.transition_seq <= floor
        || list.iter().any(|entry| {
            entry.link_revision == key.link_revision
                && entry.transition_seq == key.transition_seq
                && entry.kind == key.kind
        })
}

pub(crate) fn notification_claimed(
    conn: &Connection,
    thread_id: &str,
    key: &NotificationKey,
) -> Result<bool, StorageError> {
    check_id("thread_id", thread_id)?;
    check_notification_key(key)?;
    let (list, floor, _) = notified_state(conn, thread_id)?;
    Ok(is_claimed(&list, floor, key))
}

pub(crate) fn claim_notification(
    conn: &Connection,
    thread_id: &str,
    key: &NotificationKey,
) -> Result<bool, StorageError> {
    check_id("thread_id", thread_id)?;
    check_notification_key(key)?;
    let tx = immediate(conn)?;
    tombstone_check(&tx, thread_id)?;
    bound_link(&tx, thread_id, key.link_revision)?;
    let (mut list, mut floor, issued) = notified_state(&tx, thread_id)?;
    if key.transition_seq > issued {
        return Err(invalid("transition_seq", "was never issued"));
    }
    if is_claimed(&list, floor, key) {
        return Ok(false);
    }
    list.push(Notified {
        link_revision: key.link_revision,
        transition_seq: key.transition_seq,
        kind: key.kind.clone(),
    });
    if list.len() > PULL_REQUEST_NOTIFIED_MAX {
        list.sort_by_key(|entry| entry.transition_seq);
        let excess = list.len() - PULL_REQUEST_NOTIFIED_MAX;
        for dropped in list.drain(..excess) {
            floor = floor.max(dropped.transition_seq);
        }
    }
    let json = serde_json::to_string(&list)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    tx.execute(
        "UPDATE thread_pull_request_state SET notified_json = ?2, notified_floor_seq = ?3 \
         WHERE thread_id = ?1",
        params![thread_id, json, floor],
    )?;
    tx.commit()?;
    Ok(true)
}

fn candidate_by_key(
    conn: &Connection,
    thread_id: &str,
    key: &PullRequestKey,
) -> Result<Option<PullRequestCandidate>, StorageError> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {CANDIDATE_COLUMNS} FROM thread_pull_request_candidates \
                 WHERE thread_id = ?1 AND provider = ?2 AND host = ?3 AND repository = ?4 \
                   AND number = ?5"
            ),
            params![
                thread_id,
                key.provider.as_str(),
                key.host,
                key.repository,
                key.number
            ],
            candidate_from_row,
        )
        .optional()?)
}

pub(crate) fn present_candidate(
    conn: &Connection,
    new: &NewPullRequestCandidate,
) -> Result<CandidatePresented, StorageError> {
    check_id("thread_id", &new.thread_id)?;
    check_id("source_run_id", &new.source_run_id)?;
    check_id("source_call_id", &new.source_call_id)?;
    let key = new.key.normalized()?;
    let tx = immediate(conn)?;
    tombstone_check(&tx, &new.thread_id)?;
    let run_deleted: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id = ?1)",
        [&new.source_run_id],
        |row| row.get(0),
    )?;
    if run_deleted {
        return Err(StorageError::Tombstoned {
            run_id: Some(new.source_run_id.clone()),
            thread_id: None,
        });
    }
    if let Some(existing) = candidate_by_key(&tx, &new.thread_id, &key)? {
        return Ok(CandidatePresented {
            candidate: existing,
            created: false,
        });
    }
    let held: usize = tx.query_row(
        "SELECT COUNT(*) FROM thread_pull_request_candidates WHERE thread_id = ?1",
        [&new.thread_id],
        |row| row.get(0),
    )?;
    // A decided candidate is the memory of the decision: dropping it would offer the same
    // pull request again. So a full thread refuses new candidates instead of making room.
    if held >= PULL_REQUEST_CANDIDATES_MAX {
        return Err(StorageError::PullRequest(
            PullRequestRejection::CandidateLimit,
        ));
    }
    tx.execute(
        "INSERT INTO thread_pull_request_candidates (thread_id, provider, host, repository, \
            number, source_run_id, source_call_id, state, created_at_ns, updated_at_ns) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'presented', ?8, ?8)",
        params![
            new.thread_id,
            key.provider.as_str(),
            key.host,
            key.repository,
            key.number,
            new.source_run_id,
            new.source_call_id,
            new.at_ns,
        ],
    )?;
    let candidate = candidate_by_key(&tx, &new.thread_id, &key)?
        .ok_or(StorageError::OutOfRange("pull request candidate"))?;
    tx.commit()?;
    Ok(CandidatePresented {
        candidate,
        created: true,
    })
}

pub(crate) fn dismiss_candidate(
    conn: &Connection,
    thread_id: &str,
    candidate_id: u64,
    at_ns: i64,
) -> Result<PullRequestCandidate, StorageError> {
    check_id("thread_id", thread_id)?;
    let tx = immediate(conn)?;
    tombstone_check(&tx, thread_id)?;
    tx.execute(
        "UPDATE thread_pull_request_candidates SET state = 'dismissed', updated_at_ns = ?3 \
         WHERE candidate_id = ?1 AND thread_id = ?2 AND state = 'presented'",
        params![candidate_id, thread_id, at_ns],
    )?;
    let candidate = tx
        .query_row(
            &format!(
                "SELECT {CANDIDATE_COLUMNS} FROM thread_pull_request_candidates \
                 WHERE candidate_id = ?1 AND thread_id = ?2"
            ),
            params![candidate_id, thread_id],
            candidate_from_row,
        )
        .optional()?
        .ok_or(StorageError::PullRequest(
            PullRequestRejection::CandidateNotFound,
        ))?;
    tx.commit()?;
    Ok(candidate)
}

pub(crate) fn candidates(
    conn: &Connection,
    thread_id: &str,
    state: Option<CandidateState>,
) -> Result<Vec<PullRequestCandidate>, StorageError> {
    check_id("thread_id", thread_id)?;
    let mut statement = conn.prepare(&format!(
        "SELECT {CANDIDATE_COLUMNS} FROM thread_pull_request_candidates \
         WHERE thread_id = ?1 AND (?2 IS NULL OR state = ?2) ORDER BY candidate_id"
    ))?;
    Ok(statement
        .query_map(
            params![thread_id, state.map(CandidateState::as_str)],
            candidate_from_row,
        )?
        .collect::<Result<_, _>>()?)
}
