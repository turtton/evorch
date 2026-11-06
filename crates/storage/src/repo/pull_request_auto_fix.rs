//! PR semantic identity and preparation, not a second delivery engine.
use super::pull_requests::{active_link, check_id, immediate, tombstone_check};
use crate::{
    StorageError, lane_delivery::*, pull_request_auto_fix::*, pull_requests::PullRequestKey,
};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
mod delivery;
fn err(reason: &str) -> StorageError {
    StorageError::Serialization(format!("PR notice: {reason}"))
}
fn json(value: &impl serde::Serialize) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|e| err(&e.to_string()))
}
fn parse<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, StorageError> {
    serde_json::from_str(value).map_err(|e| err(&e.to_string()))
}
fn key_id(key: &PullRequestKey) -> Result<String, StorageError> {
    let key = key.normalized()?;
    json(&(key.provider.as_str(), key.host, key.repository, key.number))
}
fn ensure(conn: &Connection, key: &PullRequestKey) -> Result<(), StorageError> {
    conn.execute(
        "INSERT OR IGNORE INTO pr_auto_fix_state(pr_key) VALUES (?1)",
        [key_id(key)?],
    )?;
    Ok(())
}
pub(crate) fn state(
    conn: &Connection,
    key: &PullRequestKey,
) -> Result<PrAutoFixState, StorageError> {
    let key = key.normalized()?;
    let result=conn.query_row("SELECT enabled,destination_thread_id,link_revision,policy_revision,budget_epoch,used,cooldown_until_ns,stop_reason,limit_reached_epoch,(SELECT COUNT(*) FROM pr_notices n WHERE n.pr_key=p.pr_key AND n.budget_epoch=p.budget_epoch AND n.mode='auto' AND n.phase IN ('reserved','ready')) FROM pr_auto_fix_state p WHERE pr_key=?1",[key_id(&key)?],|r|Ok(PrAutoFixState { key:key.clone(),enabled:r.get(0)?,destination_thread_id:r.get(1)?,link_revision:r.get(2)?,policy_revision:r.get(3)?,budget_epoch:r.get(4)?,used:r.get(5)?,cooldown_until_ns:r.get(6)?,stop_reason:r.get(7)?,limit_reached_epoch:r.get(8)?,reserved:r.get(9)? })).optional()?;
    Ok(result.unwrap_or(PrAutoFixState {
        key,
        enabled: false,
        destination_thread_id: None,
        link_revision: None,
        policy_revision: 0,
        budget_epoch: 0,
        used: 0,
        reserved: 0,
        cooldown_until_ns: None,
        stop_reason: None,
        limit_reached_epoch: None,
    }))
}
fn check_link(
    conn: &Connection,
    key: &PullRequestKey,
    thread: &str,
    revision: u64,
) -> Result<(), StorageError> {
    check_id("thread_id", thread)?;
    tombstone_check(conn, thread)?;
    let archived: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM threads WHERE thread_id=?1 AND archived=1)",
        [thread],
        |r| r.get(0),
    )?;
    if archived {
        return Err(StorageError::PrAutoFix(
            PrAutoFixRejection::ArchivedDestination,
        ));
    }
    let link = active_link(conn, thread)?
        .ok_or_else(|| StorageError::PrAutoFix(PrAutoFixRejection::NotLinked))?;
    if link.key != key.normalized()? || link.link_revision != revision {
        return Err(StorageError::PrAutoFix(PrAutoFixRejection::StaleLink));
    }
    Ok(())
}
fn policy_revision(actual: u64, expected: u64) -> Result<(), StorageError> {
    if actual == expected {
        Ok(())
    } else {
        Err(StorageError::StaleRevision {
            kind: "pr_auto_fix_policy",
            expected,
            current: actual,
        })
    }
}
fn cancel_auto(conn: &Connection, key: &str) -> Result<(), StorageError> {
    conn.execute("UPDATE lane_notifications SET state='obsolete',claim_epoch=NULL WHERE source_kind='pull_request_checks_failed' AND receipt_prompt_id IS NULL AND notification_id IN (SELECT notice_id FROM pr_notices WHERE pr_key=?1 AND mode='auto' AND phase IN ('reserved','ready'))",[key])?;
    conn.execute("UPDATE pr_notices SET phase='cancelled',payload=NULL WHERE pr_key=?1 AND mode='auto' AND phase IN ('reserved','ready')",[key])?;
    Ok(())
}
pub(crate) fn update_policy(
    conn: &Connection,
    request: PrPolicyUpdate,
) -> Result<PrAutoFixState, StorageError> {
    let key = request.key.normalized()?;
    let tx = immediate(conn)?;
    ensure(&tx, &key)?;
    let current = state(&tx, &key)?;
    policy_revision(current.policy_revision, request.expected_policy_revision)?;
    if current.destination_thread_id != request.expected_destination_thread_id {
        return Err(StorageError::PrAutoFix(
            PrAutoFixRejection::DestinationConfirmationRequired,
        ));
    }
    let enabled = !matches!(request.action, PrPolicyAction::SetEnabled(false));
    if enabled {
        check_link(
            &tx,
            &key,
            &request.destination_thread_id,
            request.link_revision,
        )?;
    }
    let reset = matches!(request.action, PrPolicyAction::ResetBudget);
    if enabled && !reset && current.used >= PR_AUTO_FIX_LIMIT {
        return Err(StorageError::PrAutoFix(
            PrAutoFixRejection::BudgetResetRequired,
        ));
    }
    let revision = current
        .policy_revision
        .checked_add(1)
        .ok_or(StorageError::OutOfRange("policy revision"))?;
    let epoch = current
        .budget_epoch
        .checked_add(u64::from(reset))
        .ok_or(StorageError::OutOfRange("budget epoch"))?;
    cancel_auto(&tx, &key_id(&key)?)?;
    tx.execute("UPDATE pr_auto_fix_state SET enabled=?2,destination_thread_id=CASE WHEN ?2 THEN ?3 ELSE destination_thread_id END,link_revision=CASE WHEN ?2 THEN ?4 ELSE link_revision END,policy_revision=?5,budget_epoch=?6,used=CASE WHEN ?7 THEN 0 ELSE used END,stop_reason=CASE WHEN ?2 THEN NULL ELSE 'disabled' END WHERE pr_key=?1",params![key_id(&key)?,enabled,request.destination_thread_id,request.link_revision,revision,epoch,reset])?;
    let result = state(&tx, &key)?;
    tx.commit()?;
    Ok(result)
}
fn auto_allowed(
    state: &PrAutoFixState,
    thread: &str,
    link_revision: u64,
    global: bool,
    now: i64,
) -> Result<(), StorageError> {
    if !global || !state.enabled {
        return Err(StorageError::PrAutoFix(PrAutoFixRejection::AutoFixDisabled));
    }
    if state.destination_thread_id.as_deref() != Some(thread)
        || state.link_revision != Some(link_revision)
    {
        return Err(StorageError::PrAutoFix(
            PrAutoFixRejection::WrongDestination,
        ));
    }
    if state.used >= PR_AUTO_FIX_LIMIT {
        return Err(StorageError::PrAutoFix(PrAutoFixRejection::BudgetExhausted));
    }
    if state.cooldown_until_ns.is_some_and(|until| now < until) {
        return Err(StorageError::PrAutoFix(PrAutoFixRejection::Cooldown));
    }
    Ok(())
}
const NOTICE_COLUMNS: &str = "notice_id,pr_key,head_sha,mode,destination_thread_id,link_revision,policy_revision,budget_epoch,success_seq,phase,payload";
fn notice_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PrNotice> {
    let key: String = r.get(1)?;
    let (provider, host, repository, number): (String, String, String, u64) =
        serde_json::from_str(&key).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
        })?;
    let mode: String = r.get(3)?;
    let phase: String = r.get(9)?;
    Ok(PrNotice {
        notice_id: r.get(0)?,
        key: PullRequestKey {
            provider: crate::pull_requests::PullRequestProvider::parse(&provider)
                .ok_or(rusqlite::Error::InvalidQuery)?,
            host,
            repository,
            number,
        },
        head_sha: r.get(2)?,
        mode: match mode.as_str() {
            "manual" => PrNoticeMode::Manual,
            "auto" => PrNoticeMode::Auto,
            _ => return Err(rusqlite::Error::InvalidQuery),
        },
        destination_thread_id: r.get(4)?,
        link_revision: r.get(5)?,
        policy_revision: r.get(6)?,
        budget_epoch: r.get(7)?,
        success_seq: r.get(8)?,
        phase: match phase.as_str() {
            "reserved" => PrNoticePhase::Reserved,
            "ready" => PrNoticePhase::Ready,
            "adopted" => PrNoticePhase::Adopted,
            "cancelled" => PrNoticePhase::Cancelled,
            _ => return Err(rusqlite::Error::InvalidQuery),
        },
        payload: r.get(10)?,
    })
}
pub(crate) fn notice(conn: &Connection, id: &str) -> Result<Option<PrNotice>, StorageError> {
    Ok(conn
        .query_row(
            &format!("SELECT {NOTICE_COLUMNS} FROM pr_notices WHERE notice_id=?1"),
            [id],
            notice_row,
        )
        .optional()?)
}
fn required(conn: &Connection, id: &str) -> Result<PrNotice, StorageError> {
    notice(conn, id)?.ok_or_else(|| StorageError::PrAutoFix(PrAutoFixRejection::UnknownNotice))
}
pub(crate) fn reserve(
    conn: &Connection,
    request: PrNoticeReservation,
) -> Result<PrNotice, StorageError> {
    let key = request.key.normalized()?;
    let sha = request.head_sha.to_ascii_lowercase();
    if !matches!(sha.len(), 40 | 64) || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(StorageError::PrAutoFix(PrAutoFixRejection::InvalidHeadSha));
    }
    let tx = immediate(conn)?;
    check_link(
        &tx,
        &key,
        &request.destination_thread_id,
        request.link_revision,
    )?;
    ensure(&tx, &key)?;
    let current = state(&tx, &key)?;
    let id = format!(
        "prn-{:x}",
        Sha256::digest(json(&(key_id(&key)?, &sha))?.as_bytes())
    );
    if let Some(old) = notice(&tx, &id)?
        && old.phase != PrNoticePhase::Cancelled
    {
        if old.phase == PrNoticePhase::Adopted {
            return Err(StorageError::PrAutoFix(PrAutoFixRejection::AlreadyAdopted));
        }
        if old.mode != request.mode
            || old.destination_thread_id.as_deref() != Some(&request.destination_thread_id)
            || old.link_revision != request.link_revision
            || old.success_seq != request.success_seq
        {
            return Err(StorageError::PrAutoFix(
                PrAutoFixRejection::ReservationConflict,
            ));
        }
        return Ok(old);
    }
    policy_revision(current.policy_revision, request.expected_policy_revision)?;
    if request.mode == PrNoticeMode::Auto {
        auto_allowed(
            &current,
            &request.destination_thread_id,
            request.link_revision,
            request.auto_fix_enabled,
            request.now_ns,
        )?;
        if current.used + current.reserved >= PR_AUTO_FIX_LIMIT {
            return Err(StorageError::PrAutoFix(PrAutoFixRejection::BudgetReserved));
        }
    }
    tx.execute("INSERT INTO pr_notices(notice_id,pr_key,head_sha,mode,destination_thread_id,link_revision,policy_revision,budget_epoch,success_seq,phase) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'reserved') ON CONFLICT(pr_key,head_sha) DO UPDATE SET mode=excluded.mode,destination_thread_id=excluded.destination_thread_id,link_revision=excluded.link_revision,policy_revision=excluded.policy_revision,budget_epoch=excluded.budget_epoch,success_seq=excluded.success_seq,phase='reserved',payload=NULL",params![id,key_id(&key)?,sha,match request.mode{PrNoticeMode::Manual=>"manual",PrNoticeMode::Auto=>"auto"},request.destination_thread_id,request.link_revision,current.policy_revision,current.budget_epoch,request.success_seq])?;
    let result = required(&tx, &id)?;
    tx.commit()?;
    Ok(result)
}
pub(crate) fn ready(conn: &Connection, id: &str, payload: &str) -> Result<PrNotice, StorageError> {
    if payload.len() > PR_NOTICE_PAYLOAD_MAX {
        return Err(StorageError::PrAutoFix(PrAutoFixRejection::PayloadTooLarge));
    }
    crate::entity::SecretGuard::from_env().check_text("pr_notice", "payload", payload)?;
    let tx = immediate(conn)?;
    let current = required(&tx, id)?;
    if current.phase == PrNoticePhase::Ready && current.payload.as_deref() == Some(payload) {
        return Ok(current);
    }
    if current.phase != PrNoticePhase::Reserved {
        return Err(StorageError::PrAutoFix(PrAutoFixRejection::NotReserved));
    }
    let thread = current
        .destination_thread_id
        .as_deref()
        .ok_or_else(|| StorageError::PrAutoFix(PrAutoFixRejection::NoDestination))?;
    check_link(&tx, &current.key, thread, current.link_revision)?;
    let reference = PrNoticeRef {
        notification_id: id.into(),
        destination_thread_id: thread.into(),
    };
    tx.execute(
        "UPDATE pr_notices SET phase='ready',payload=?2 WHERE notice_id=?1",
        params![id, payload],
    )?;
    tx.execute("INSERT INTO lane_notifications(notification_id,source_kind,pr_key,management_revision,event_kind,semantic_key,source_thread_id,destination_thread_id,payload) VALUES (?1,'pull_request_checks_failed',?2,1,'pr_checks_failed',?3,?4,?4,?5) ON CONFLICT(notification_id) DO UPDATE SET state='pending',claim_epoch=NULL,source_thread_id=excluded.source_thread_id,destination_thread_id=excluded.destination_thread_id,payload=excluded.payload",params![id,key_id(&current.key)?,current.head_sha,thread,json(&reference)?])?;
    let result = required(&tx, id)?;
    tx.commit()?;
    Ok(result)
}
pub(crate) fn cancel(conn: &Connection, id: &str) -> Result<(), StorageError> {
    let tx = immediate(conn)?;
    let current = required(&tx, id)?;
    if current.phase == PrNoticePhase::Adopted {
        return Err(StorageError::PrAutoFix(PrAutoFixRejection::AlreadyAdopted));
    }
    tx.execute("UPDATE lane_notifications SET state='obsolete',claim_epoch=NULL WHERE notification_id=?1 AND receipt_prompt_id IS NULL",[id])?;
    tx.execute(
        "UPDATE pr_notices SET phase='cancelled',payload=NULL WHERE notice_id=?1",
        [id],
    )?;
    tx.commit()?;
    Ok(())
}
pub(crate) fn guard(
    conn: &Connection,
    key: &PullRequestKey,
) -> Result<LaneWakeGuard, StorageError> {
    let value: Option<String> = conn
        .query_row(
            "SELECT wake_guard_json FROM pr_auto_fix_state WHERE pr_key=?1",
            [key_id(key)?],
            |r| r.get(0),
        )
        .optional()?;
    value
        .as_deref()
        .map(parse)
        .transpose()
        .map(Option::unwrap_or_default)
}
pub(crate) fn update_guard_in_transaction(
    conn: &Connection,
    key: &PullRequestKey,
    expected: u64,
    signal: PrGuardSignal,
) -> Result<LaneWakeGuard, StorageError> {
    ensure(conn, key)?;
    let signal = match signal {
        PrGuardSignal::WakeBatch { batch_id } => LaneGuardSignal::WakeBatch { batch_id },
        PrGuardSignal::RealUserInput => LaneGuardSignal::RealUserInput,
        PrGuardSignal::ExplicitUserResume => LaneGuardSignal::ExplicitUserResume,
    };
    let before = guard(conn, key)?;
    let state = super::lane_delivery::advance_guard(before.clone(), expected, signal)?;
    if state != before {
        conn.execute(
            "UPDATE pr_auto_fix_state SET wake_guard_json=?2 WHERE pr_key=?1",
            params![key_id(key)?, json(&state)?],
        )?;
    }
    Ok(state)
}
pub(crate) fn update_guard(
    conn: &Connection,
    key: &PullRequestKey,
    expected: u64,
    signal: PrGuardSignal,
) -> Result<LaneWakeGuard, StorageError> {
    let tx = immediate(conn)?;
    let result = update_guard_in_transaction(&tx, key, expected, signal)?;
    tx.commit()?;
    Ok(result)
}

/// An actual user receipt resets sources linked to this input's thread, never Stop.
pub(crate) fn notify_user_input(
    conn: &Connection,
    receipt: &crate::HostInputReceipt,
) -> Result<(), StorageError> {
    if receipt.provenance != crate::HostInputProvenance::User {
        return Ok(());
    }
    let mut q=conn.prepare("SELECT DISTINCT provider,host,repository,number FROM thread_pull_requests WHERE superseded_at_ns IS NULL AND thread_id IN (SELECT json_extract(config_json,'$.thread_id') FROM run_contexts WHERE run_id=?1 UNION SELECT thread_id FROM thread_runs WHERE run_id=?1) AND thread_id NOT IN (SELECT thread_id FROM thread_tombstones)")?;
    let keys = q
        .query_map([&receipt.root_run_id], |r| {
            super::pull_requests::key_at(r, 0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for key in keys {
        let revision = guard(conn, &key)?.revision;
        update_guard_in_transaction(conn, &key, revision, PrGuardSignal::RealUserInput)?;
    }
    Ok(())
}
pub(crate) fn list_notices(
    conn: &Connection,
    key: &PullRequestKey,
    after: &str,
) -> Result<Vec<PrNotice>, StorageError> {
    let mut q=conn.prepare(&format!("SELECT {NOTICE_COLUMNS} FROM pr_notices WHERE pr_key=?1 AND notice_id>?2 ORDER BY notice_id LIMIT 20"))?;
    Ok(q.query_map(params![key_id(key)?, after], notice_row)?
        .collect::<Result<_, _>>()?)
}
pub(crate) fn pending_destinations(
    conn: &Connection,
    after: &str,
) -> Result<Vec<String>, StorageError> {
    let mut q = conn.prepare(
        "SELECT thread_id FROM (
           SELECT destination_thread_id AS thread_id FROM lane_notifications
           WHERE source_kind='pull_request_checks_failed' AND state='pending'
             AND destination_thread_id NOT IN (SELECT thread_id FROM thread_tombstones)
           UNION
           SELECT d.thread_id FROM lane_delivery_destinations d
           JOIN run_contexts c ON c.run_id=json_extract(d.owner_json,'$.root_run_id')
           WHERE d.stopped=0 AND c.parent_run_id IS NULL
             AND c.terminal_phase IN ('Checkpoint','Error','Done')
             AND c.run_id NOT IN (SELECT run_id FROM run_tombstones)
             AND d.thread_id NOT IN (SELECT thread_id FROM thread_tombstones)
             AND CASE WHEN json_valid(c.config_json)
               THEN json_extract(c.config_json,'$.harness_notice_pending')=1 ELSE 0 END
         ) WHERE thread_id>?1 ORDER BY thread_id LIMIT 20",
    )?;
    Ok(q.query_map([after], |r| r.get(0))?
        .collect::<Result<_, _>>()?)
}

pub(crate) fn limit_events(
    c: &Connection,
    key: &PullRequestKey,
    after: Option<u64>,
) -> Result<Vec<PrAutoFixLimitEvent>, StorageError> {
    let key = key.normalized()?;
    let mut q=c.prepare("SELECT budget_epoch,notice_id,at_ns FROM pr_auto_fix_limit_events WHERE pr_key=?1 AND (?2 IS NULL OR budget_epoch>?2) ORDER BY budget_epoch LIMIT 20")?;
    Ok(q.query_map(params![key_id(&key)?, after], |r| {
        Ok(PrAutoFixLimitEvent {
            key: key.clone(),
            budget_epoch: r.get(0)?,
            notice_id: r.get(1)?,
            at_ns: r.get(2)?,
        })
    })?
    .collect::<Result<_, _>>()?)
}
