use super::*;
use rusqlite::params;

const GRANT_COLUMNS: &str = "invocation_id,restart_id,capture_id,state,issued_at_ms,\
     ready_deadline_ms,ready_at_ms,reconnect_deadline_ms,ended_at_ms";
const TERMINAL_PHASES: [&str; 4] = ["completed", "aborted", "invalidated", "superseded"];

fn corrupt(reason: &str) -> StorageError {
    StorageError::Serialization(reason.into())
}

fn state_of(text: &str) -> Result<GrantState, StorageError> {
    Ok(match text {
        "issued" => GrantState::Issued,
        "ready" => GrantState::Ready,
        "redeemed" => GrantState::Redeemed,
        "revoked" => GrantState::Revoked,
        "expired" => GrantState::Expired,
        _ => return Err(corrupt("unknown continuity state")),
    })
}

fn view(r: &rusqlite::Row<'_>) -> Result<GrantView, StorageError> {
    let state: String = r.get(3)?;
    Ok(GrantView {
        binding: ContinuityBinding {
            invocation_id: r.get(0)?,
            restart_id: r.get(1)?,
            capture_id: r.get(2)?,
        },
        state: state_of(&state)?,
        issued_at_ms: r.get(4)?,
        ready_deadline_ms: r.get(5)?,
        ready_at_ms: r.get(6)?,
        reconnect_deadline_ms: r.get(7)?,
        ended_at_ms: r.get(8)?,
    })
}

pub(super) fn grant(
    conn: &Connection,
    invocation_id: &str,
    restart_id: &str,
) -> Result<Option<GrantView>, StorageError> {
    let mut statement = conn.prepare(&format!(
        "SELECT {GRANT_COLUMNS} FROM cli_continuity_grants WHERE invocation_id=?1 AND restart_id=?2"
    ))?;
    let mut rows = statement.query(params![invocation_id, restart_id])?;
    rows.next()?.map(view).transpose()
}

pub(super) fn handoff_grants(
    conn: &Connection,
    restart_id: &str,
) -> Result<HandoffGrants, StorageError> {
    let mut statement = conn.prepare(&format!(
        "SELECT {GRANT_COLUMNS} FROM cli_continuity_grants WHERE restart_id=?1 ORDER BY rowid LIMIT ?2"
    ))?;
    let mut rows = statement.query(params![restart_id, (MAX_LIVE_GRANTS + 1) as i64])?;
    let mut grants = Vec::new();
    while let Some(row) = rows.next()? {
        grants.push(view(row)?);
    }
    let truncated = grants.len() > MAX_LIVE_GRANTS;
    grants.truncate(MAX_LIVE_GRANTS);
    Ok(HandoffGrants { grants, truncated })
}

/// Takes `now_ms` into the writer's clock. A time below the high-water mark is refused
/// before anything else is read or written.
fn observe(tx: &Connection, now_ms: u64) -> Result<ContinuityReport<()>, StorageError> {
    if let Err(refusal) = check_time(now_ms) {
        return Ok(Err(refusal));
    }
    let high_water: u64 = tx
        .query_row(
            "SELECT high_water_ms FROM cli_continuity_clock WHERE id=1",
            [],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| corrupt("continuity clock is missing"))?;
    if now_ms < high_water {
        return Ok(Err(ContinuityRefusal::ClockRegressed));
    }
    if now_ms > high_water {
        tx.execute(
            "UPDATE cli_continuity_clock SET high_water_ms=?1 WHERE id=1",
            [now_ms],
        )?;
    }
    Ok(Ok(()))
}

const DUE: &str = "((state='issued' AND ready_deadline_ms<=?1) \
     OR (state='ready' AND reconnect_deadline_ms<=?1))";

/// Ends every live grant whose deadline is at or before `now_ms`. The deadline instant
/// itself is already too late.
fn sweep(tx: &Connection, now_ms: u64) -> Result<Vec<ContinuityBinding>, StorageError> {
    let due: Vec<ContinuityBinding> = tx
        .prepare(&format!(
            "SELECT invocation_id,restart_id,capture_id FROM cli_continuity_grants \
             WHERE {DUE} ORDER BY rowid"
        ))?
        .query_map([now_ms], binding_row)?
        .collect::<Result<_, _>>()?;
    if !due.is_empty() {
        tx.execute(
            &format!("UPDATE cli_continuity_grants SET state='expired',ended_at_ms=?1 WHERE {DUE}"),
            [now_ms],
        )?;
    }
    Ok(due)
}

fn binding_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ContinuityBinding> {
    Ok(ContinuityBinding {
        invocation_id: r.get(0)?,
        restart_id: r.get(1)?,
        capture_id: r.get(2)?,
    })
}

/// Whether the restart transaction already ended, or why it is not the handoff asked for.
fn handoff(
    tx: &Connection,
    restart_id: &str,
    capture_id: &str,
) -> Result<ContinuityReport<bool>, StorageError> {
    let found: Option<(String, String)> = tx
        .query_row(
            "SELECT capture_id,phase FROM restart_transactions WHERE restart_id=?1",
            [restart_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(match found {
        None => Err(ContinuityRefusal::HandoffNotFound),
        Some((capture, _)) if capture != capture_id => Err(ContinuityRefusal::HandoffMismatch),
        Some((_, phase)) => Ok(TERMINAL_PHASES.contains(&phase.as_str())),
    })
}

fn live_count(tx: &Connection) -> Result<usize, StorageError> {
    Ok(tx.query_row(
        "SELECT count(*) FROM cli_continuity_grants WHERE state IN ('issued','ready')",
        [],
        |r| r.get::<_, i64>(0),
    )? as usize)
}

macro_rules! refuse {
    ($expr:expr) => {
        match $expr {
            Ok(value) => value,
            Err(refusal) => return Ok(Err(refusal)),
        }
    };
}

pub(super) fn issue(
    tx: &Connection,
    binding: &ContinuityBinding,
    token: &ContinuityToken,
    now_ms: u64,
) -> Result<ContinuityReport<GrantView>, StorageError> {
    refuse!(check_binding(binding));
    refuse!(observe(tx, now_ms)?);
    sweep(tx, now_ms)?;
    if refuse!(handoff(tx, &binding.restart_id, &binding.capture_id)?) {
        return Ok(Err(ContinuityRefusal::HandoffEnded));
    }
    if grant(tx, &binding.invocation_id, &binding.restart_id)?.is_some() {
        return Ok(Err(ContinuityRefusal::AlreadyIssued));
    }
    if live_count(tx)? >= MAX_LIVE_GRANTS {
        return Ok(Err(ContinuityRefusal::TooManyLive));
    }
    let digest = token.digest(binding);
    tx.execute(
        "INSERT INTO cli_continuity_grants(invocation_id,restart_id,capture_id,token_digest,\
         issued_at_ms,ready_deadline_ms,state) VALUES(?1,?2,?3,?4,?5,?6,'issued')",
        params![
            binding.invocation_id,
            binding.restart_id,
            binding.capture_id,
            digest.as_slice(),
            now_ms,
            now_ms + RESTART_TO_READY_MS,
        ],
    )?;
    let issued = grant(tx, &binding.invocation_id, &binding.restart_id)?
        .ok_or_else(|| corrupt("issued continuity grant is missing"))?;
    Ok(Ok(issued))
}

pub(super) fn mark_ready(
    tx: &Connection,
    restart_id: &str,
    capture_id: &str,
    now_ms: u64,
) -> Result<ContinuityReport<HandoffReady>, StorageError> {
    refuse!(check_id(restart_id, "restart_id"));
    refuse!(check_id(capture_id, "capture_id"));
    refuse!(observe(tx, now_ms)?);
    let expired: Vec<ContinuityBinding> = sweep(tx, now_ms)?
        .into_iter()
        .filter(|b| b.restart_id == restart_id)
        .collect();
    refuse!(handoff(tx, restart_id, capture_id)?);
    let waiting: Vec<ContinuityBinding> = tx
        .prepare(
            "SELECT invocation_id,restart_id,capture_id FROM cli_continuity_grants \
             WHERE restart_id=?1 AND capture_id=?2 AND state='issued' ORDER BY rowid",
        )?
        .query_map([restart_id, capture_id], binding_row)?
        .collect::<Result<_, _>>()?;
    for grant in &waiting {
        tx.execute(
            "UPDATE cli_continuity_grants SET state='ready',ready_at_ms=?3,\
             reconnect_deadline_ms=?3+30000 WHERE invocation_id=?1 AND restart_id=?2",
            params![grant.invocation_id, grant.restart_id, now_ms],
        )?;
    }
    Ok(Ok(HandoffReady {
        ready: waiting,
        expired,
    }))
}

/// Digest equality without an early exit.
fn same_digest(left: &[u8], right: &[u8; 32]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

pub(super) fn redeem(
    tx: &Connection,
    binding: &ContinuityBinding,
    token: &ContinuityToken,
    now_ms: u64,
) -> Result<ContinuityReport<GrantView>, StorageError> {
    refuse!(check_binding(binding));
    refuse!(observe(tx, now_ms)?);
    sweep(tx, now_ms)?;
    let stored: Option<(String, Vec<u8>)> = tx
        .query_row(
            "SELECT capture_id,token_digest FROM cli_continuity_grants \
             WHERE invocation_id=?1 AND restart_id=?2",
            [&binding.invocation_id, &binding.restart_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((capture, digest)) = stored else {
        return Ok(Err(ContinuityRefusal::NotFound));
    };
    if capture != binding.capture_id {
        return Ok(Err(ContinuityRefusal::HandoffMismatch));
    }
    if !same_digest(&digest, &token.digest(binding)) {
        return Ok(Err(ContinuityRefusal::TokenMismatch));
    }
    let current = grant(tx, &binding.invocation_id, &binding.restart_id)?
        .ok_or_else(|| corrupt("continuity grant vanished"))?;
    match current.state {
        GrantState::Issued => return Ok(Err(ContinuityRefusal::NotReady)),
        GrantState::Redeemed => return Ok(Err(ContinuityRefusal::AlreadyRedeemed)),
        GrantState::Revoked => return Ok(Err(ContinuityRefusal::Revoked)),
        GrantState::Expired => return Ok(Err(ContinuityRefusal::Expired)),
        GrantState::Ready => {}
    }
    let changed = tx.execute(
        "UPDATE cli_continuity_grants SET state='redeemed',ended_at_ms=?3 \
         WHERE invocation_id=?1 AND restart_id=?2 AND state='ready'",
        params![binding.invocation_id, binding.restart_id, now_ms],
    )?;
    if changed != 1 {
        return Err(corrupt("continuity grant changed while redeeming"));
    }
    let redeemed = grant(tx, &binding.invocation_id, &binding.restart_id)?
        .ok_or_else(|| corrupt("continuity grant vanished"))?;
    Ok(Ok(redeemed))
}

pub(super) fn revoke(
    tx: &Connection,
    scope: &RevokeScope,
    now_ms: u64,
) -> Result<ContinuityReport<RevokeReport>, StorageError> {
    match scope {
        RevokeScope::Grant {
            invocation_id,
            restart_id,
        } => {
            refuse!(check_id(invocation_id, "invocation_id"));
            refuse!(check_id(restart_id, "restart_id"));
        }
        RevokeScope::Invocation { invocation_id } => {
            refuse!(check_id(invocation_id, "invocation_id"));
        }
        RevokeScope::Handoff { restart_id } => refuse!(check_id(restart_id, "restart_id")),
    }
    refuse!(observe(tx, now_ms)?);
    sweep(tx, now_ms)?;
    let candidates: Vec<GrantView> = match scope {
        RevokeScope::Grant {
            invocation_id,
            restart_id,
        } => match grant(tx, invocation_id, restart_id)? {
            None => return Ok(Err(ContinuityRefusal::NotFound)),
            Some(g) if g.state == GrantState::Redeemed => {
                return Ok(Err(ContinuityRefusal::AlreadyRedeemed));
            }
            Some(g) => vec![g],
        },
        RevokeScope::Invocation { invocation_id } => {
            select_live(tx, "invocation_id=?1", invocation_id)?
        }
        RevokeScope::Handoff { restart_id } => select_live(tx, "restart_id=?1", restart_id)?,
    };
    let mut revoked = Vec::new();
    for grant in candidates.into_iter().filter(|g| g.state.live()) {
        tx.execute(
            "UPDATE cli_continuity_grants SET state='revoked',ended_at_ms=?3 \
             WHERE invocation_id=?1 AND restart_id=?2",
            params![
                grant.binding.invocation_id,
                grant.binding.restart_id,
                now_ms
            ],
        )?;
        revoked.push(grant.binding);
    }
    Ok(Ok(RevokeReport { revoked }))
}

fn select_live(tx: &Connection, filter: &str, key: &str) -> Result<Vec<GrantView>, StorageError> {
    let mut statement = tx.prepare(&format!(
        "SELECT {GRANT_COLUMNS} FROM cli_continuity_grants \
         WHERE state IN ('issued','ready') AND {filter} ORDER BY rowid"
    ))?;
    let mut rows = statement.query([key])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(view(row)?);
    }
    Ok(out)
}

pub(super) fn expire_due(
    tx: &Connection,
    now_ms: u64,
) -> Result<ContinuityReport<Vec<ContinuityBinding>>, StorageError> {
    refuse!(observe(tx, now_ms)?);
    Ok(Ok(sweep(tx, now_ms)?))
}
