//! Durable retry policy for actual restore failures. Claims only fence/idempotently
//! identify an attempt; they are deliberately absent from recovery identity.
use super::*;
const COLUMNS: &str = "episode_id,destination_thread_id,saved_root_run_id,stable_recovery_target,attempts,last_attempt_at_ns,state,last_failure_class,revision,failure_report_ref";
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<LaneRecoveryEpisode> {
    let state: String = r.get(6)?;
    let state = match state.as_str() {
        "retryable" => LaneRecoveryState::Retryable,
        "suppressed" => LaneRecoveryState::Suppressed,
        "resolved" => LaneRecoveryState::Resolved,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(LaneRecoveryEpisode {
        episode_id: r.get(0)?,
        destination_thread_id: r.get(1)?,
        saved_root_run_id: r.get(2)?,
        stable_recovery_target: r.get(3)?,
        attempts: r.get(4)?,
        last_attempt_at_ns: r.get(5)?,
        state,
        last_failure_class: r.get(7)?,
        revision: r.get(8)?,
        failure_report_ref: r.get(9)?,
    })
}
pub(crate) fn recovery_episode(
    conn: &Connection,
    episode: &str,
) -> Result<Option<LaneRecoveryEpisode>, StorageError> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM lane_recovery_episodes WHERE episode_id=?1"),
            [episode],
            row,
        )
        .optional()?)
}
pub(crate) fn active_recovery(
    conn: &Connection,
    destination: &str,
) -> Result<Option<LaneRecoveryEpisode>, StorageError> {
    Ok(conn.query_row(&format!("SELECT {COLUMNS} FROM lane_recovery_episodes WHERE destination_thread_id=?1 AND state<>'resolved'"), [destination], row).optional()?)
}
pub(crate) fn latest_recovery(
    conn: &Connection,
    destination: &str,
) -> Result<Option<LaneRecoveryEpisode>, StorageError> {
    Ok(conn.query_row(&format!("SELECT {COLUMNS} FROM lane_recovery_episodes WHERE destination_thread_id=?1 ORDER BY sequence DESC LIMIT 1"), [destination], row).optional()?)
}
pub(crate) fn resolve_recovery_destination(
    conn: &Connection,
    destination: &str,
) -> Result<(), StorageError> {
    let changed = conn.execute("UPDATE lane_recovery_episodes SET state='resolved',revision=revision+1 WHERE destination_thread_id=?1 AND state<>'resolved'", [destination])?;
    if changed > 0 {
        conn.execute("UPDATE lane_delivery_destinations SET last_nack=NULL WHERE thread_id=?1 AND last_nack='\"restore_failed\"'", [destination])?;
    }
    Ok(())
}
fn recovery_target(conn: &Connection, root: &str) -> Result<String, StorageError> {
    let deleted: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=?1)",
        [root],
        |r| r.get(0),
    )?;
    if deleted {
        return Err(conflict("recovery_target_missing"));
    }
    let r = crate::repo::run_context::get(conn, root)?
        .ok_or_else(|| conflict("recovery_target_missing"))?;
    // Bookkeeping timestamps, report sequence, claims and ownership do not enter
    // this hash. A changed source snapshot/configuration is a NEW episode.
    Ok(format!(
        "snapshot:{:x}",
        Sha256::digest(
            json(&(
                &r.run_id,
                &r.config_json,
                &r.messages_json,
                &r.checkpoints_json,
                &r.host_inputs_json,
                r.restorable,
            ))?
            .as_bytes()
        )
    ))
}
pub(crate) fn prepare_recovery(
    conn: &Connection,
    claim: &LaneDeliveryClaim,
    now: i64,
) -> Result<LaneRecoveryAttempt, StorageError> {
    let tx = transaction(conn)?;
    delivery::check_claim(&tx, claim, now)?;
    let destination = destination(&tx, &claim.destination_thread_id)?
        .ok_or_else(|| conflict("stale_destination_epoch"))?;
    let result = prepare_recovery_source(&tx, &destination, claim.token.clone())?;
    tx.commit()?;
    Ok(result)
}
fn prepare_recovery_source(
    tx: &Transaction<'_>,
    destination: &LaneDeliveryDestination,
    token: String,
) -> Result<LaneRecoveryAttempt, StorageError> {
    if destination.stopped {
        return Err(conflict("stopped"));
    }
    let owner = destination
        .owner
        .as_ref()
        .ok_or_else(|| conflict("stale_owner"))?;
    let target = recovery_target(tx, &owner.root_run_id)?;
    let old = active_recovery(tx, &destination.thread_id)?;
    let episode = if let Some(e) = old.filter(|e| e.stable_recovery_target == target) {
        e
    } else {
        resolve_recovery_destination(tx, &destination.thread_id)?;
        let sequence: u64 = tx.query_row(
            "SELECT COALESCE(MAX(sequence),0)+1 FROM lane_recovery_episodes",
            [],
            |r| r.get(0),
        )?;
        let episode_id = format!(
            "lr-{:x}",
            Sha256::digest(json(&(&destination.thread_id, &target, sequence))?.as_bytes())
        );
        tx.execute("INSERT INTO lane_recovery_episodes(sequence,episode_id,destination_thread_id,saved_root_run_id,stable_recovery_target,state,revision) VALUES(?1,?2,?3,?4,?5,'retryable',1)", params![sequence, episode_id, destination.thread_id, owner.root_run_id, target])?;
        recovery_episode(tx, &episode_id)?.ok_or_else(|| conflict("missing_recovery_episode"))?
    };
    let report_target = crate::repo::lanes::get_by_thread(tx, &destination.thread_id)?.map(|l| {
        LaneRecoveryReportTarget {
            lane_id: l.lane_id,
            management_revision: l.management_revision,
            generation: l.generation,
            report_seq: l.latest_report_seq,
        }
    });
    Ok(LaneRecoveryAttempt {
        destination_epoch: destination.epoch,
        owner: owner.clone(),
        token,
        episode,
        report_target,
    })
}
pub(crate) fn fail_recovery(
    conn: &Connection,
    claim: &LaneDeliveryClaim,
    attempt: LaneRecoveryAttempt,
    now: i64,
) -> Result<LaneRecoveryFailure, StorageError> {
    fail_recovery_source(conn, Some(claim), attempt, now)
}
fn fail_recovery_source(
    conn: &Connection,
    claim: Option<&LaneDeliveryClaim>,
    attempt: LaneRecoveryAttempt,
    now: i64,
) -> Result<LaneRecoveryFailure, StorageError> {
    let tx = transaction(conn)?;
    let mut e = recovery_episode(&tx, &attempt.episode.episode_id)?
        .ok_or_else(|| conflict("stale_recovery"))?;
    let token: Option<String> = tx.query_row(
        "SELECT last_attempt_token FROM lane_recovery_episodes WHERE episode_id=?1",
        [&e.episode_id],
        |r| r.get(0),
    )?;
    if token.as_deref() == Some(&attempt.token) {
        return Ok(LaneRecoveryFailure {
            episode: e,
            report: None,
        });
    }
    let destination =
        destination(&tx, &e.destination_thread_id)?.ok_or_else(|| conflict("stale_recovery"))?;
    if destination.stopped
        || destination.epoch != attempt.destination_epoch
        || destination.owner.as_ref() != Some(&attempt.owner)
    {
        return Err(conflict("stale_recovery"));
    }
    if let Some(claim) = claim {
        if attempt.token != claim.token || e.destination_thread_id != claim.destination_thread_id {
            return Err(conflict("stale_recovery"));
        }
        delivery::check_claim(&tx, claim, now)?;
    }
    if e.state != LaneRecoveryState::Retryable
        || e.revision != attempt.episode.revision
        || e.stable_recovery_target != recovery_target(&tx, &attempt.owner.root_run_id)?
    {
        return Err(conflict("stale_recovery"));
    }
    e.attempts += 1;
    e.last_attempt_at_ns = Some(now);
    e.last_failure_class = Some("restore_failed".into());
    e.revision += 1;
    if e.attempts >= LANE_RESTORE_FAILURE_LIMIT {
        e.state = LaneRecoveryState::Suppressed;
    }
    let mut report = None;
    if e.failure_report_ref.is_none()
        && let Some(expected) = attempt.report_target
    {
        let l = lane(&tx, &expected.lane_id)?;
        // A late failure may not overwrite Ready, a new generation or new work.
        let ready: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM lane_reports WHERE lane_id=?1 AND report_seq=?2 AND status='ready')", params![l.lane_id, l.latest_report_seq], |r| r.get(0))?;
        if l.policy.is_managed()
            && !ready
            && l.root_run_id.as_deref() == Some(&attempt.owner.root_run_id)
            && l.management_revision == expected.management_revision
            && l.generation == expected.generation
            && l.latest_report_seq == expected.report_seq
        {
            let appended = append_report_in_transaction(&tx, &l.lane_id, l.management_revision, event_bus::LaneReportInput {
                status: event_bus::LaneStatus::Failed,
                summary: "Runtime recovery failed; inspect the recovery episode or explicitly Resume.".into(),
                artifact_refs: vec![], artifact_revision: None, failure_episode: Some(e.episode_id.clone()),
                needs_human: false, question_refs: vec![], reason_code: Some("restore_failed".into()),
                producer: event_bus::LaneReportProducer::Harness,
            }, now)?;
            e.failure_report_ref = Some(format!(
                "{}:{}",
                appended.report.lane_id, appended.report.report_seq
            ));
            report = Some(appended);
        }
    }
    tx.execute("UPDATE lane_recovery_episodes SET attempts=?2,last_attempt_at_ns=?3,state=?4,last_failure_class='restore_failed',revision=?5,failure_report_ref=?6,last_attempt_token=?7 WHERE episode_id=?1", params![e.episode_id, e.attempts, now, if e.state == LaneRecoveryState::Suppressed {"suppressed"} else {"retryable"}, e.revision, e.failure_report_ref, attempt.token])?;
    tx.commit()?;
    Ok(LaneRecoveryFailure { episode: e, report })
}
pub(crate) fn resolve_recovery(
    conn: &Connection,
    attempt: LaneRecoveryAttempt,
) -> Result<(), StorageError> {
    let tx = transaction(conn)?;
    if destination(&tx, &attempt.episode.destination_thread_id)?.is_some_and(|d| {
        !d.stopped
            && d.epoch == attempt.destination_epoch
            && d.owner.as_ref() == Some(&attempt.owner)
    }) {
        tx.execute("UPDATE lane_recovery_episodes SET state='resolved',revision=revision+1 WHERE episode_id=?1 AND revision=?2 AND state<>'resolved'", params![attempt.episode.episode_id, attempt.episode.revision])?;
    }
    tx.commit()?;
    Ok(())
}

/// Recovery of already-accepted presentation/receipt work has no new claim.
pub(crate) fn prepare_destination_recovery(
    conn: &Connection,
    thread: &str,
    expected_epoch: u64,
    owner: LaneDeliveryOwner,
    token: String,
) -> Result<LaneRecoveryAttempt, StorageError> {
    let tx = transaction(conn)?;
    let d = destination(&tx, thread)?.ok_or_else(|| conflict("stale_destination_epoch"))?;
    if d.epoch != expected_epoch || d.owner.as_ref() != Some(&owner) {
        return Err(conflict("stale_destination_epoch"));
    }
    id(&token)?;
    let prepared = prepare_recovery_source(&tx, &d, token)?;
    tx.commit()?;
    Ok(prepared)
}
pub(crate) fn fail_destination_recovery(
    conn: &Connection,
    attempt: LaneRecoveryAttempt,
    now: i64,
) -> Result<LaneRecoveryFailure, StorageError> {
    fail_recovery_source(conn, None, attempt, now)
}
