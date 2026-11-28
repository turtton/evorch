use super::*;
use graph_control::{GraphControlRecord, GraphEffectKey};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

const SELECT_GRAPHS: &str = "SELECT c.restart_id,c.manifest_id,c.disposition,c.reason,c.revision,\
     t.capture_id,t.phase,t.adopted_epoch \
     FROM restart_components c JOIN restart_transactions t ON t.restart_id=c.restart_id \
     WHERE c.disposition='quarantined'";

struct Row {
    restart_id: String,
    manifest_id: String,
    reason: Option<String>,
    revision: u64,
    capture_id: String,
    phase: String,
    adopted_epoch: Option<u64>,
}
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok(Row {
        restart_id: r.get(0)?,
        manifest_id: r.get(1)?,
        reason: r.get(3)?,
        revision: r.get(4)?,
        capture_id: r.get(5)?,
        phase: r.get(6)?,
        adopted_epoch: r.get(7)?,
    })
}

pub(super) fn snapshot(
    conn: &Connection,
    limit: usize,
) -> Result<QuarantineSnapshot, StorageError> {
    let take = limit.saturating_add(1);
    let rows: Vec<Row> = conn
        .prepare(&format!("{SELECT_GRAPHS} ORDER BY c.rowid DESC LIMIT ?1"))?
        .query_map([take as i64], row)?
        .collect::<Result<_, _>>()?;
    let truncated = rows.len() > limit;
    let graphs = rows
        .into_iter()
        .take(limit)
        .map(|r| build(conn, r))
        .collect::<Result<_, _>>()?;
    Ok(QuarantineSnapshot { graphs, truncated })
}

pub(super) fn graph(
    conn: &Connection,
    restart_id: &str,
    manifest_id: &str,
) -> Result<Option<QuarantinedGraph>, StorageError> {
    conn.query_row(
        &format!("{SELECT_GRAPHS} AND c.restart_id=?1 AND c.manifest_id=?2"),
        [restart_id, manifest_id],
        row,
    )
    .optional()?
    .map(|r| build(conn, r))
    .transpose()
}

fn reason(text: Option<&str>) -> QuarantineReason {
    match text {
        Some(COLD_REASON) => QuarantineReason::ColdBoot,
        Some(RELEASED_WITHOUT_CLAIM_REASON) => QuarantineReason::ReleasedWithoutClaim,
        _ => QuarantineReason::Other,
    }
}

fn claim_state(conn: &Connection, manifest: &str) -> Result<QuarantineClaim, StorageError> {
    let states: Vec<String> = conn
        .prepare("SELECT state FROM recovery_claims WHERE manifest_id=?1")?
        .query_map([manifest], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(if states.iter().any(|s| s == "activated") {
        QuarantineClaim::Activated
    } else if states.iter().any(|s| s == "prepared") {
        QuarantineClaim::Prepared
    } else if states.is_empty() {
        QuarantineClaim::Absent
    } else {
        QuarantineClaim::Released
    })
}

fn build(conn: &Connection, row: Row) -> Result<QuarantinedGraph, StorageError> {
    let control = graph_control::control(conn, &row.manifest_id)?;
    let claim = claim_state(conn, &row.manifest_id)?;
    let progress: Vec<Progress> = conn
        .prepare(
            "SELECT thread_id,scratch,revision FROM recovery_graph_thread_progress WHERE manifest_id=?1",
        )?
        .query_map([&row.manifest_id], |r| {
            Ok(Progress {
                thread: r.get(0)?,
                scratch: r.get(1)?,
                revision: r.get(2)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let threads = threads(conn, &row.restart_id, &row.manifest_id)?;
    let members: u32 = conn.query_row(
        "SELECT COUNT(*) FROM restart_expected_members WHERE restart_id=?1 AND manifest_id=?2",
        [&row.restart_id, &row.manifest_id],
        |r| r.get(0),
    )?;
    let revoked = control.as_ref().is_some_and(|c| c.revoked);
    let revision = progress
        .iter()
        .try_fold(
            row.revision
                .checked_add(control.as_ref().map_or(0, |c| c.revision)),
            |sum, p| Some(sum?.checked_add(p.revision)),
        )
        .flatten()
        .ok_or_else(|| invalid("quarantine revision exhausted"))?;
    let completion = completion(revoked, claim, threads.as_ref(), &progress);
    Ok(QuarantinedGraph {
        restart_id: row.restart_id,
        capture_id: row.capture_id,
        manifest_id: row.manifest_id,
        revision,
        cold: row.adopted_epoch.is_none(),
        terminal: matches!(
            row.phase.as_str(),
            "completed" | "aborted" | "invalidated" | "superseded"
        ),
        reason: reason(row.reason.as_deref()),
        claim,
        revoked,
        completion,
        members,
        control,
    })
}

struct Progress {
    thread: String,
    scratch: String,
    revision: u64,
}

/// The threads the graph's members belong to: each may hold an owner and a
/// scratch that this leaf can neither inspect nor clean. A member record that
/// cannot be read leaves the set unknown.
fn threads(
    conn: &Connection,
    restart: &str,
    manifest: &str,
) -> Result<Result<BTreeSet<String>, ()>, StorageError> {
    let members: Vec<String> = conn
        .prepare(
            "SELECT member_json FROM restart_expected_members WHERE restart_id=?1 AND manifest_id=?2",
        )?
        .query_map([restart, manifest], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(members
        .iter()
        .map(|json| {
            serde_json::from_str::<restart::RestartExpectedMember>(json)
                .map(|m| m.thread_id)
                .map_err(|_| ())
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|ids| ids.into_iter().flatten().collect()))
}

/// Complete needs evidence for everything: no prepared claim, and every thread
/// of the graph recorded as finished (applied, or not applicable) for both its
/// owners and its scratch. No record is not evidence that nothing remains: a
/// thread without one is not started, and an unreadable or unknown record is
/// unknown. A graph whose members belong to no thread has nothing to clean.
fn completion(
    revoked: bool,
    claim: QuarantineClaim,
    threads: Result<&BTreeSet<String>, &()>,
    progress: &[Progress],
) -> QuarantineCompletion {
    if !revoked {
        return QuarantineCompletion::NotStarted;
    }
    let mut result = match claim {
        QuarantineClaim::Absent | QuarantineClaim::Released => QuarantineCompletion::Complete,
        QuarantineClaim::Prepared => QuarantineCompletion::Partial,
        QuarantineClaim::Activated => return QuarantineCompletion::Unknown,
    };
    let Ok(threads) = threads else {
        return QuarantineCompletion::Unknown;
    };
    for step in progress.iter().map(|p| &p.scratch) {
        match step.as_str() {
            "applied" | "not_applicable" => {}
            "not_started" => result = QuarantineCompletion::Partial,
            _ => return QuarantineCompletion::Unknown,
        }
    }
    if threads
        .iter()
        .any(|thread| !progress.iter().any(|p| &p.thread == thread))
    {
        result = QuarantineCompletion::Partial;
    }
    result
}

fn digest(request: &DiscardRequest) -> String {
    // The attempt nonce is excluded: one operation repeated is one request.
    let mut hash = Sha256::new();
    for part in [
        "quarantine_discard",
        &request.operation_id,
        &request.restart_id,
        &request.capture_id,
        &request.manifest_id,
        &request.expected_revision.to_string(),
    ] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

pub(super) fn discard(
    conn: &Connection,
    request: &DiscardRequest,
) -> Result<DiscardReport, StorageError> {
    for id in [&request.operation_id, &request.attempt_nonce] {
        if id.is_empty() || id.len() > 128 {
            return Err(invalid("invalid graph effect key"));
        }
    }
    let previous: i64 = conn.pragma_query_value(None, "synchronous", |r| r.get(0))?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    let result = transact(conn, request);
    let restore = conn.pragma_update(None, "synchronous", previous);
    let result = result?;
    restore?;
    Ok(result)
}

fn transact(conn: &Connection, request: &DiscardRequest) -> Result<DiscardReport, StorageError> {
    let tx = conn.unchecked_transaction()?;
    let request_digest = digest(request);
    let prior: Option<String> = tx
        .query_row(
            "SELECT request_digest FROM recovery_graph_effects WHERE operation_id=?1",
            [&request.operation_id],
            |r| r.get(0),
        )
        .optional()?;
    let current = graph(&tx, &request.restart_id, &request.manifest_id)?;
    if let Some(prior) = prior {
        return Ok(match (prior == request_digest, current) {
            (true, Some(graph)) => Ok(DiscardResult {
                change: DiscardChange::Replayed,
                claim_release: ClaimRelease::NotNeeded,
                graph,
            }),
            (true, None) => Err(DiscardRefusal::NotFound),
            (false, _) => Err(DiscardRefusal::OperationConflict),
        });
    }
    let Some(found) = current else {
        return Ok(Err(DiscardRefusal::NotFound));
    };
    if found.capture_id != request.capture_id || found.revision != request.expected_revision {
        return Ok(Err(DiscardRefusal::Conflict));
    }
    if !found.cold {
        return Ok(Err(DiscardRefusal::NotCold));
    }
    if !found.terminal {
        return Ok(Err(DiscardRefusal::NotTerminal));
    }
    if found.control.as_ref().is_some_and(|c| !c.revoked) {
        return Ok(Err(DiscardRefusal::ManualInProgress));
    }
    let prepared_claim: Option<String> = tx
        .query_row(
            "SELECT claim_id FROM recovery_claims WHERE manifest_id=?1 AND state='prepared'",
            [&request.manifest_id],
            |r| r.get(0),
        )
        .optional()?;
    let mut change = DiscardChange::AlreadyRevoked;
    if found.control.is_none() {
        let origin = if prepared_claim.is_some() {
            "manual_release"
        } else {
            "manual_release_without_claim"
        };
        tx.execute(
            "INSERT INTO recovery_graph_control(manifest_id,mode,revision,operation_id,attempt_nonce,revoked,origin) \
             VALUES(?1,'revoked_idle',1,?2,?3,1,?4)",
            rusqlite::params![
                request.manifest_id,
                request.operation_id,
                request.attempt_nonce,
                origin
            ],
        )?;
        change = DiscardChange::Revoked;
    }
    // The revocation above stands even when the claim cannot be released: the
    // two are observed separately and a failed release stays Partial.
    let claim_release = match prepared_claim {
        None => ClaimRelease::NotNeeded,
        Some(claim) => {
            tx.execute_batch("SAVEPOINT quarantine_claim")?;
            match prepared::release(&tx, &claim, DISCARD_RELEASE_REASON) {
                Ok(_) => {
                    tx.execute_batch("RELEASE quarantine_claim")?;
                    ClaimRelease::Released
                }
                Err(_) => {
                    tx.execute_batch("ROLLBACK TO quarantine_claim; RELEASE quarantine_claim")?;
                    ClaimRelease::Failed
                }
            }
        }
    };
    let control: GraphControlRecord = graph_control::control(&tx, &request.manifest_id)?
        .ok_or_else(|| invalid("missing graph control"))?;
    let effect = GraphEffectKey {
        operation_id: request.operation_id.clone(),
        attempt_nonce: request.attempt_nonce.clone(),
    };
    tx.execute(
        "INSERT INTO recovery_graph_effects(operation_id,attempt_nonce,manifest_id,request_digest,result_json) \
         VALUES(?1,?2,?3,?4,?5)",
        rusqlite::params![
            effect.operation_id,
            effect.attempt_nonce,
            request.manifest_id,
            request_digest,
            serde_json::to_string(&control).map_err(|_| invalid("invalid graph control record"))?
        ],
    )?;
    let graph = graph(&tx, &request.restart_id, &request.manifest_id)?
        .ok_or_else(|| invalid("quarantined graph disappeared"))?;
    tx.commit()?;
    Ok(Ok(DiscardResult {
        change,
        claim_release,
        graph,
    }))
}
